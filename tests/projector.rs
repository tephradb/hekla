//! A projector reads through the event envelope: the shared `envelope::decode`
//! must unwrap `.data` in `project_to_head` (not only in the command fold), or a
//! projector would see the metadata wrapper instead of the payload.

use kiln::projector::project_to_head;
use kiln::read_model::ReadModel;
use kiln::starlark_builtins::{EmittedEvent, EntityOpKind, ModuleDef};
use serde_json::json;
use uuid::Uuid;

mod support;

use support::{
    ALICE, Boot, MISSING, UUID_A, UUID_B, UUID_C, ctx, example_dir, load_ok, log_head, open_store,
    seed_event, write_project,
};

#[test]
fn projector_reads_through_the_envelope() {
    let project = load_ok(&example_dir("users"));

    let projector = project
        .projectors
        .iter()
        .find(|unit| unit.loaded.def.name() == "users")
        .expect("users projector");
    let ModuleDef::Projector { entities, .. } = &projector.loaded.def else {
        panic!("expected a projector");
    };

    let store_dir = tempfile::tempdir().unwrap();
    let (coordinator, store) = open_store(store_dir.path());

    // Append a real, envelope-wrapped event through the same seam a command uses.
    let ctx = ctx();
    seed_event(
        &store,
        &project,
        &ctx,
        EmittedEvent {
            event_type: "user.registered".to_owned(),
            data: json!({ "user_id": ALICE, "email": "alice@example.com", "name": "Alice" }),
            tags: vec![
                ("user_id".to_owned(), Some(ALICE.to_owned())),
                ("email".to_owned(), Some("alice@example.com".to_owned())),
            ],
        },
    );

    let model_dir = tempfile::tempdir().unwrap();
    let model = ReadModel::open(&model_dir.path().join("users.db"), entities).unwrap();
    let seen = project_to_head(&store, &projector.loaded, &model, &project.events.by_type).unwrap();
    assert_eq!(seen, 1);
    assert_eq!(model.read_checkpoint().unwrap().get(), 1);

    let entity = entities
        .iter()
        .find(|entity| entity.name == "users")
        .unwrap();
    let rows = model.rows(entity).unwrap();
    assert_eq!(rows.len(), 1);
    // The payload fields, not the envelope, landed in the read model.
    assert_eq!(rows[0]["email"], "alice@example.com");
    assert_eq!(rows[0]["name"], "Alice");
    assert_eq!(rows[0]["user_id"], ALICE);
    coordinator.shutdown();
}

const THING_EVENTS: &str = r#"
happened = event(
    type = "thing.happened",
    fields = {"id": uuid()},
)
"#;

#[test]
fn get_reads_through_uncommitted_writes_in_a_batch() {
    // A projector that keeps a running count with get()+put(): the second event in
    // a batch must observe the first event's still-uncommitted write, or the total
    // would land at 1 instead of 2.
    let dir = write_project(&[
        ("events/thing.star", THING_EVENTS),
        (
            "projectors/counter.star",
            r#"
load("events/thing.star", "happened")

totals = entity(key = "id", fields = {"id": str(), "count": int()})

def on_event(event):
    row = get(totals, "all")
    count = (row["count"] if row else 0) + 1
    return [put(totals, {"id": "all", "count": count})]

handle = {happened(): on_event}
"#,
        ),
    ]);

    let project = load_ok(dir.path());
    let projector = &project.projectors[0];
    let ModuleDef::Projector { entities, .. } = &projector.loaded.def else {
        panic!("expected a projector");
    };

    let store_dir = tempfile::tempdir().unwrap();
    let (coordinator, store) = open_store(store_dir.path());

    let ctx = ctx();
    for _ in 0..2 {
        let id = Uuid::new_v4().to_string();
        seed_event(
            &store,
            &project,
            &ctx,
            EmittedEvent {
                event_type: "thing.happened".to_owned(),
                data: json!({ "id": id }),
                tags: vec![("id".to_owned(), Some(id.clone()))],
            },
        );
    }

    let model_dir = tempfile::tempdir().unwrap();
    let model = ReadModel::open(&model_dir.path().join("counter.db"), entities).unwrap();
    let seen = project_to_head(&store, &projector.loaded, &model, &project.events.by_type).unwrap();
    assert_eq!(seen, 2);

    let entity = entities.iter().find(|e| e.name == "totals").unwrap();
    let row = model.get(entity, "all").unwrap().unwrap();
    assert_eq!(row["count"].as_i64(), Some(2));
    coordinator.shutdown();
}

#[test]
fn a_failed_op_names_the_entity_it_was_applying() {
    // A read model whose table predates a new entity field: the INSERT names a column
    // the table does not have. The failure reaches /status verbatim, so it has to say
    // which entity was being written, not just the bare SQLite message.
    let dir = write_project(&[
        ("events/thing.star", THING_EVENTS),
        (
            "projectors/things.star",
            r#"
load("events/thing.star", "happened")

rows = entity(key = "id", fields = {"id": str(), "label": str()})

def on_event(event):
    return [put(rows, {"id": event.data.id, "label": "x"})]

handle = {happened(): on_event}
"#,
        ),
    ]);

    let project = load_ok(dir.path());
    let projector = &project.projectors[0];
    let ModuleDef::Projector { entities, .. } = &projector.loaded.def else {
        panic!("expected a projector");
    };

    let store_dir = tempfile::tempdir().unwrap();
    let (coordinator, store) = open_store(store_dir.path());

    let ctx = ctx();
    let id = Uuid::new_v4().to_string();
    seed_event(
        &store,
        &project,
        &ctx,
        EmittedEvent {
            event_type: "thing.happened".to_owned(),
            data: json!({ "id": id }),
            tags: vec![("id".to_owned(), Some(id.clone()))],
        },
    );

    // Open the read model at the old shape, before `label` was declared.
    let mut stale = entities[0].clone();
    stale.fields.retain(|(name, _)| name != "label");
    let model_dir = tempfile::tempdir().unwrap();
    let model = ReadModel::open(&model_dir.path().join("things.db"), &[stale]).unwrap();

    let err = project_to_head(&store, &projector.loaded, &model, &project.events.by_type)
        .expect_err("the insert names a column the stale table does not have");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("applying an op to entity `rows`"),
        "{rendered}"
    );
    coordinator.shutdown();
}

const BIG_EVENTS: &str = r#"
counted = event(
    type = "big.counted",
    fields = {"id": uuid(), "n": uint()},
)
"#;

const BIG_PROJECTOR: &str = r#"
load("events/big.star", "counted")

nums = entity(key = "id", fields = {"id": uuid(), "n": uint()})

def on_event(event):
    return [put(nums, {"id": event.data.id, "n": event.data.n})]

handle = {counted(): on_event}
"#;

/// Project one `big.counted` carrying `n` and read the stored value back.
fn project_one_u64(n: u64) -> serde_json::Value {
    let dir = write_project(&[
        ("events/big.star", BIG_EVENTS),
        ("projectors/big.star", BIG_PROJECTOR),
    ]);
    let project = load_ok(dir.path());
    let projector = &project.projectors[0];
    let ModuleDef::Projector { entities, .. } = &projector.loaded.def else {
        panic!("expected a projector");
    };

    let store_dir = tempfile::tempdir().unwrap();
    let (coordinator, store) = open_store(store_dir.path());
    let ctx = ctx();
    seed_event(
        &store,
        &project,
        &ctx,
        EmittedEvent {
            event_type: "big.counted".to_owned(),
            data: json!({ "id": UUID_A, "n": n }),
            tags: vec![("id".to_owned(), Some(UUID_A.to_owned()))],
        },
    );

    let model_dir = tempfile::tempdir().unwrap();
    let model = ReadModel::open(&model_dir.path().join("big.db"), entities).unwrap();
    let seen = project_to_head(&store, &projector.loaded, &model, &project.events.by_type).unwrap();
    assert_eq!(seen, 1);
    let entity = entities.iter().find(|e| e.name == "nums").unwrap();
    let read_back = model.get(entity, UUID_A).unwrap().expect("the row landed");
    coordinator.shutdown();
    read_back
}

#[test]
fn a_u64_field_at_exactly_i64_max_round_trips_through_the_read_model() {
    // `i64::MAX` is the top of the storable range for either integer kind, since both
    // land in a signed SQLite INTEGER. Nothing above it ever reaches a projector: the
    // write boundary refuses it, which the command test below pins.
    let row = project_one_u64(i64::MAX as u64);
    assert_eq!(row["n"].as_u64(), Some(i64::MAX as u64));
}

const BIG_COMMAND: &str = r#"
load("events/big.star", "counted")

input = schema(id = uuid(), n = uint())

def handle(input, state):
    return counted(id = input.id, n = input.n)
"#;

#[test]
fn a_u64_above_i64_max_is_refused_at_the_command_boundary() {
    // The other end of that range, end to end: a u64 a signed INTEGER cannot hold is
    // invalid input, refused before any append, rather than a value that reaches
    // `to_sql` and wedges the projector thread on every replay.
    let dir = write_project(&[
        ("events/big.star", BIG_EVENTS),
        ("commands/count.star", BIG_COMMAND),
        ("projectors/big.star", BIG_PROJECTOR),
    ]);
    let harness = Boot::new(dir.path()).start();

    let result = harness
        .rt
        .execute(
            "count",
            json!({ "id": UUID_A, "n": u64::MAX }),
            &ctx(),
            None,
        )
        .unwrap();

    assert_eq!(result.status, 400, "{:?}", result.body);
    assert_eq!(result.body["error"]["code"], "invalid_input");
    let message = result.body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("`n`"),
        "the error names the field: {message}"
    );
    assert!(
        message.contains(&i64::MAX.to_string()),
        "the error names the storable ceiling: {message}"
    );
    assert_eq!(
        log_head(&harness.rt),
        0,
        "invalid input must not reach the log"
    );

    harness.shutdown();
}

const LIFECYCLE_EVENTS: &str = r#"
added = event(type = "thing.added", fields = {"id": uuid()})
removed = event(type = "thing.removed", fields = {"id": uuid()})
"#;

const LIFECYCLE_PROJECTOR: &str = r#"
load("events/thing.star", "added", "removed")

things = entity(key = "id", fields = {"id": uuid()})

handle = {
    added(): lambda event: [put(things, {"id": event.data.id})],
    removed(): lambda event: [delete(things, event.data.id)],
}
"#;

#[test]
fn a_delete_op_removes_the_row_and_is_a_no_op_for_a_missing_key() {
    let dir = write_project(&[
        ("events/thing.star", LIFECYCLE_EVENTS),
        ("projectors/things.star", LIFECYCLE_PROJECTOR),
    ]);
    let project = load_ok(dir.path());
    let projector = &project.projectors[0];
    let ModuleDef::Projector { entities, .. } = &projector.loaded.def else {
        panic!("expected a projector");
    };

    let store_dir = tempfile::tempdir().unwrap();
    let (coordinator, store) = open_store(store_dir.path());
    let ctx = ctx();
    // C is removed without ever having been added: the delete must find no row and
    // stay silent rather than erroring and wedging the batch.
    for (event_type, id) in [
        ("thing.added", UUID_A),
        ("thing.added", UUID_B),
        ("thing.removed", UUID_A),
        ("thing.removed", UUID_C),
    ] {
        seed_event(
            &store,
            &project,
            &ctx,
            EmittedEvent {
                event_type: event_type.to_owned(),
                data: json!({ "id": id }),
                tags: vec![("id".to_owned(), Some(id.to_owned()))],
            },
        );
    }

    let model_dir = tempfile::tempdir().unwrap();
    let model = ReadModel::open(&model_dir.path().join("things.db"), entities).unwrap();
    let seen = project_to_head(&store, &projector.loaded, &model, &project.events.by_type).unwrap();
    assert_eq!(seen, 4);

    let entity = entities.iter().find(|e| e.name == "things").unwrap();
    assert!(
        model.get(entity, UUID_A).unwrap().is_none(),
        "the deleted row is gone"
    );
    assert!(
        model.get(entity, UUID_B).unwrap().is_some(),
        "an untouched row survives its sibling's delete"
    );
    assert_eq!(model.rows(entity).unwrap().len(), 1);
    coordinator.shutdown();
}

const RENAME_EVENTS: &str = r#"
registered = event(type = "u.registered", fields = {"id": uuid(), "name": str()})
renamed = event(type = "u.renamed", fields = {"id": uuid(), "name": str()})
"#;

const RENAME_PROJECTOR: &str = r#"
load("events/u.star", "registered", "renamed")

people = entity(key = "id", fields = {"id": uuid(), "name": str()})

handle = {
    registered(): lambda event: [put(people, {"id": event.data.id, "name": event.data.name})],
    renamed(): lambda event: [patch(people, event.data.id, {"name": event.data.name})],
}
"#;

#[test]
fn a_patch_for_a_missing_row_is_a_silent_no_op() {
    let dir = write_project(&[
        ("events/u.star", RENAME_EVENTS),
        ("projectors/people.star", RENAME_PROJECTOR),
    ]);
    let project = load_ok(dir.path());
    let projector = &project.projectors[0];
    let ModuleDef::Projector { entities, .. } = &projector.loaded.def else {
        panic!("expected a projector");
    };

    let store_dir = tempfile::tempdir().unwrap();
    let (coordinator, store) = open_store(store_dir.path());
    let ctx = ctx();
    // The rename for MISSING arrives with no row behind it, exactly as it would for
    // a projector whose source set grew to include renames it never saw registers for.
    for (event_type, id, name) in [
        ("u.renamed", MISSING, "Ghost"),
        ("u.registered", ALICE, "Alice"),
        ("u.renamed", ALICE, "Alicia"),
    ] {
        seed_event(
            &store,
            &project,
            &ctx,
            EmittedEvent {
                event_type: event_type.to_owned(),
                data: json!({ "id": id, "name": name }),
                tags: vec![("id".to_owned(), Some(id.to_owned()))],
            },
        );
    }

    let model_dir = tempfile::tempdir().unwrap();
    let model = ReadModel::open(&model_dir.path().join("people.db"), entities).unwrap();
    let seen = project_to_head(&store, &projector.loaded, &model, &project.events.by_type).unwrap();
    assert_eq!(seen, 3);

    let entity = entities.iter().find(|e| e.name == "people").unwrap();
    assert!(
        model.get(entity, MISSING).unwrap().is_none(),
        "a patch must never fabricate the row it missed"
    );
    assert_eq!(model.rows(entity).unwrap().len(), 1);
    assert_eq!(model.get(entity, ALICE).unwrap().unwrap()["name"], "Alicia");
    coordinator.shutdown();
}

#[test]
fn a_patch_with_no_declared_columns_leaves_the_row_untouched() {
    // The `assignments.is_empty()` early return in `apply_one`: a patch whose changes
    // name nothing the entity declares must be a no-op, not an `UPDATE ... SET`
    // with an empty assignment list (a SQL syntax error that would wedge the batch).
    let dir = write_project(&[
        ("events/u.star", RENAME_EVENTS),
        ("projectors/people.star", RENAME_PROJECTOR),
    ]);
    let project = load_ok(dir.path());
    let ModuleDef::Projector { entities, .. } = &project.projectors[0].loaded.def else {
        panic!("expected a projector");
    };
    let entity = entities.iter().find(|e| e.name == "people").unwrap();

    let model_dir = tempfile::tempdir().unwrap();
    let model = ReadModel::open(&model_dir.path().join("people.db"), entities).unwrap();
    model
        .apply_one(
            entity,
            EntityOpKind::Put(json!({ "id": ALICE, "name": "Alice" }).to_string()),
        )
        .unwrap();
    model
        .apply_one(
            entity,
            EntityOpKind::Patch {
                key: ALICE.to_owned(),
                changes: "{}".to_owned(),
            },
        )
        .unwrap();

    assert_eq!(model.get(entity, ALICE).unwrap().unwrap()["name"], "Alice");
}

// --- clause-keyed handle dispatch -----------------------------------------

const PER_TYPE_EVENTS: &str = r#"
added = event(type = "thing.added", fields = {"id": uuid(), "kind": str()})
removed = event(type = "thing.removed", fields = {"id": uuid(), "kind": str()})
touched = event(type = "thing.touched", fields = {"id": uuid(), "kind": str()})
"#;

/// The keys are the subscription, so `thing.touched` is never read. Two clauses name
/// `thing.added`: the constrained one selects a subset, and both run for an event that
/// matches both.
const PER_TYPE_PROJECTOR: &str = r#"
load("events/thing.star", "added", "removed")

things = entity(key = "id", fields = {"id": uuid(), "kind": str()})
special = entity(key = "id", fields = {"id": uuid(), "kind": str()})

handle = {
    added(): lambda event: [put(things, {"id": event.data.id, "kind": event.data.kind})],
    added(kind = "vip"): lambda event: [
        put(special, {"id": event.data.id, "kind": event.data.kind}),
    ],
    removed(): lambda event: [delete(things, event.data.id)],
}
"#;

#[test]
fn a_clause_keyed_projector_handle_fans_out_and_subscribes_to_its_arms() {
    let dir = write_project(&[
        ("events/thing.star", PER_TYPE_EVENTS),
        ("projectors/things.star", PER_TYPE_PROJECTOR),
    ]);
    let project = load_ok(dir.path());
    let projector = &project.projectors[0];
    let ModuleDef::Projector { entities, .. } = &projector.loaded.def else {
        panic!("expected a projector");
    };

    let store_dir = tempfile::tempdir().unwrap();
    let (coordinator, store) = open_store(store_dir.path());
    let ctx = ctx();
    for (event_type, id, kind) in [
        ("thing.added", UUID_A, "vip"),
        ("thing.added", UUID_B, "plain"),
        // Not in any key, so the subscription skips it entirely.
        ("thing.touched", UUID_A, "vip"),
        ("thing.removed", UUID_B, "plain"),
    ] {
        seed_event(
            &store,
            &project,
            &ctx,
            EmittedEvent {
                event_type: event_type.to_owned(),
                data: json!({ "id": id, "kind": kind }),
                tags: vec![
                    ("id".to_owned(), Some(id.to_owned())),
                    ("kind".to_owned(), Some(kind.to_owned())),
                ],
            },
        );
    }

    let model_dir = tempfile::tempdir().unwrap();
    let model = ReadModel::open(&model_dir.path().join("things.db"), entities).unwrap();
    let seen = project_to_head(&store, &projector.loaded, &model, &project.events.by_type).unwrap();
    assert_eq!(seen, 3, "the unsubscribed type is never read");

    let things = entities.iter().find(|e| e.name == "things").unwrap();
    let special = entities.iter().find(|e| e.name == "special").unwrap();
    assert!(model.get(things, UUID_A).unwrap().is_some());
    assert!(
        model.get(things, UUID_B).unwrap().is_none(),
        "the removed row is gone"
    );
    // Only the vip add matched the constrained arm, and it also matched the plain one.
    assert!(model.get(special, UUID_A).unwrap().is_some());
    assert!(model.get(special, UUID_B).unwrap().is_none());
    coordinator.shutdown();
}

const ID_EVENTS: &str = r#"
happened = event(type = "thing.happened", fields = {"id": uuid()})
"#;

const ID_PROJECTOR: &str = r#"
load("events/thing.star", "happened")

things = entity(
    key = "id",
    fields = {"id": uuid(), "event_id": uuid(), "derived": uuid()},
)

handle = {
    happened(): lambda event: [put(things, {
        "id": event.data.id,
        "event_id": event.id,
        "derived": uuid5(event.id, "line-item"),
    })],
}
"#;

/// `event.id` is the envelope's id, and it does not move: rebuilding a read model
/// from position 0 has to reproduce the same value, or an id derived from it would
/// change under a replay and the rows would disagree with everything already written
/// from them.
#[test]
fn event_id_is_readable_and_survives_a_rebuild() {
    let dir = write_project(&[
        ("events/thing.star", ID_EVENTS),
        ("projectors/things.star", ID_PROJECTOR),
    ]);
    let project = load_ok(dir.path());
    let projector = &project.projectors[0];
    let ModuleDef::Projector { entities, .. } = &projector.loaded.def else {
        panic!("expected a projector");
    };

    let store_dir = tempfile::tempdir().unwrap();
    let (coordinator, store) = open_store(store_dir.path());
    seed_event(
        &store,
        &project,
        &ctx(),
        EmittedEvent {
            event_type: "thing.happened".to_owned(),
            data: json!({ "id": UUID_A }),
            tags: vec![("id".to_owned(), Some(UUID_A.to_owned()))],
        },
    );

    let project_once = || {
        let model_dir = tempfile::tempdir().unwrap();
        let model = ReadModel::open(&model_dir.path().join("things.db"), entities).unwrap();
        project_to_head(&store, &projector.loaded, &model, &project.events.by_type).unwrap();
        let things = entities.iter().find(|e| e.name == "things").unwrap();
        model.get(things, UUID_A).unwrap().expect("the row")
    };

    let row = project_once();
    let event_id = Uuid::parse_str(row["event_id"].as_str().expect("event_id is a string"))
        .expect("event.id is a uuid");
    assert_ne!(
        event_id,
        Uuid::nil(),
        "the envelope's id, not a placeholder"
    );
    assert_eq!(
        row["derived"],
        json!(Uuid::new_v5(&event_id, b"line-item").to_string()),
        "uuid5 derives RFC 4122 version 5 from the event id"
    );

    // The whole point: a fresh model built from the same log lands on the same ids.
    assert_eq!(project_once(), row);
    coordinator.shutdown();
}
