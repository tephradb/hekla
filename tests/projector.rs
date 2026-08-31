//! A projector reads through the event envelope: the shared `envelope::decode`
//! must unwrap `.data` in `project_to_head` (not only in the command fold), or a
//! projector would see the metadata wrapper instead of the payload.

use hekla::projector::project_to_head;
use hekla::read_model::ReadModel;
use hekla::schema::{EntityOpKind, ModuleDef};
use serde_json::json;
use uuid::Uuid;

mod support;

use support::{
    ALICE, MISSING, TEST_NOW, UUID_A, UUID_B, UUID_C, ctx, example_dir, load_ok, open_store,
    seed_event, write_project,
};

#[test]
fn projector_reads_through_the_envelope() {
    let project = load_ok(&example_dir("users"));

    let projector = project
        .projectors
        .iter()
        .find(|unit| unit.def.name() == "Users")
        .expect("users projector");
    let ModuleDef::Projector { entities, .. } = &projector.def else {
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
        "user.registered",
        json!({ "user_id": ALICE, "email": "alice@example.com", "name": "Alice" }),
    );

    let model_dir = tempfile::tempdir().unwrap();
    let model = ReadModel::open(&model_dir.path().join("users.db"), entities).unwrap();
    let seen = project_to_head(&store, projector, &project.program, None, &model).unwrap();
    assert_eq!(seen, 1);
    assert_eq!(model.read_checkpoint().unwrap().get(), 1);

    let entity = entities
        .iter()
        .find(|entity| entity.name == "User")
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
event @thing.happened { id: Uuid }
"#;

#[test]
fn get_reads_through_uncommitted_writes_in_a_batch() {
    // A projector that keeps a running count with get()+put(): the second event in
    // a batch must observe the first event's still-uncommitted write, or the total
    // would land at 1 instead of 2.
    let dir = write_project(&[
        ("events/thing.hk", THING_EVENTS),
        (
            "projectors/counter.hk",
            r#"
projector Counter {
  entity Totals { id: String @key @max(16), count: Int }

  // The second event in a batch has to see the first's write, or the total lands at
  // one. A stored load is what needs that, now that there is no general read.
  on @thing.happened {
    patch Totals["all"] { count: .count + 1 }
  }
}
"#,
        ),
    ]);

    let project = load_ok(dir.path());
    let projector = &project.projectors[0];
    let ModuleDef::Projector { entities, .. } = &projector.def else {
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
            "thing.happened",
            json!({ "id": id }),
        );
    }

    let model_dir = tempfile::tempdir().unwrap();
    let model = ReadModel::open(&model_dir.path().join("counter.db"), entities).unwrap();
    let seen = project_to_head(&store, projector, &project.program, None, &model).unwrap();
    assert_eq!(seen, 2);

    let entity = entities.iter().find(|e| e.name == "Totals").unwrap();
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
        ("events/thing.hk", THING_EVENTS),
        (
            "projectors/things.hk",
            r#"
projector Rows {
  entity Row { id: Uuid @key, label: String @max(8) }
  on @thing.happened { id } { put Row { id, label: "x" } }
}
"#,
        ),
    ]);

    let project = load_ok(dir.path());
    let projector = &project.projectors[0];
    let ModuleDef::Projector { entities, .. } = &projector.def else {
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
        "thing.happened",
        json!({ "id": id }),
    );

    // Open the read model at the old shape, before `label` was declared.
    let mut stale = entities[0].clone();
    stale.fields.retain(|(name, _)| name != "label");
    let model_dir = tempfile::tempdir().unwrap();
    let model = ReadModel::open(&model_dir.path().join("things.db"), &[stale]).unwrap();

    let err = project_to_head(&store, projector, &project.program, None, &model)
        .expect_err("the insert names a column the stale table does not have");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("applying a write to entity `Row`"),
        "{rendered}"
    );
    coordinator.shutdown();
}

const BIG_EVENTS: &str = r#"
event @big.counted { id: Uuid, n: Int }
"#;

const BIG_PROJECTOR: &str = r#"
projector Big {
  entity Num { id: Uuid @key, n: Int }
  on @big.counted { id, n } { put Num { id, n } }
}
"#;

/// Project one `big.counted` carrying `n` and read the stored value back.
fn project_one_u64(n: u64) -> serde_json::Value {
    let dir = write_project(&[
        ("events/big.hk", BIG_EVENTS),
        ("projectors/big.hk", BIG_PROJECTOR),
    ]);
    let project = load_ok(dir.path());
    let projector = &project.projectors[0];
    let ModuleDef::Projector { entities, .. } = &projector.def else {
        panic!("expected a projector");
    };

    let store_dir = tempfile::tempdir().unwrap();
    let (coordinator, store) = open_store(store_dir.path());
    let ctx = ctx();
    seed_event(
        &store,
        &project,
        &ctx,
        "big.counted",
        json!({ "id": UUID_A, "n": n }),
    );

    let model_dir = tempfile::tempdir().unwrap();
    let model = ReadModel::open(&model_dir.path().join("big.db"), entities).unwrap();
    let seen = project_to_head(&store, projector, &project.program, None, &model).unwrap();
    assert_eq!(seen, 1);
    let entity = entities.iter().find(|e| e.name == "Num").unwrap();
    let read_back = model.get(entity, UUID_A).unwrap().expect("the row landed");
    coordinator.shutdown();
    read_back
}

#[test]
fn an_int_at_i64_max_round_trips_through_the_read_model() {
    // `i64::MAX` is the top of the storable range for either integer kind, since both
    // land in a signed SQLite INTEGER. Nothing above it ever reaches a projector: the
    // write boundary refuses it, which the command test below pins.
    let row = project_one_u64(i64::MAX as u64);
    assert_eq!(row["n"].as_u64(), Some(i64::MAX as u64));
}

// The Starlark suite had a second test here about the `uint()` range, where a u64 a
// signed INTEGER cannot hold was refused at the write boundary. heklang has no
// unsigned type, so there is no range to fall off and nothing left to refuse. The
// `i64::MAX` round trip above is what survives of the pair.

const LIFECYCLE_EVENTS: &str = r#"
event @thing.added { id: Uuid }
event @thing.removed { id: Uuid }
"#;

const LIFECYCLE_PROJECTOR: &str = r#"
projector Things {
  entity Thing { id: Uuid @key }
  on @thing.added { id } { put Thing { id } }
  on @thing.removed { id } { delete Thing[id] }
}
"#;

#[test]
fn a_delete_op_removes_the_row_and_is_a_no_op_for_a_missing_key() {
    let dir = write_project(&[
        ("events/thing.hk", LIFECYCLE_EVENTS),
        ("projectors/things.hk", LIFECYCLE_PROJECTOR),
    ]);
    let project = load_ok(dir.path());
    let projector = &project.projectors[0];
    let ModuleDef::Projector { entities, .. } = &projector.def else {
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
        seed_event(&store, &project, &ctx, event_type, json!({ "id": id }));
    }

    let model_dir = tempfile::tempdir().unwrap();
    let model = ReadModel::open(&model_dir.path().join("things.db"), entities).unwrap();
    let seen = project_to_head(&store, projector, &project.program, None, &model).unwrap();
    assert_eq!(seen, 4);

    let entity = entities.iter().find(|e| e.name == "Thing").unwrap();
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
event @u.registered { id: Uuid, name: String @max(50) }
event @u.renamed { id: Uuid, name: String @max(50) }
"#;

const RENAME_PROJECTOR: &str = r#"
projector People {
  entity Person { id: Uuid @key, name: String @max(50) }

  on @u.registered { id, name } { put Person { id, name } }
  // `update`, which is what hekla's `patch` used to be: a rename for a row that is
  // not there writes nothing rather than materializing one from zeros.
  on @u.renamed { id, name } { update Person[id] { name } }
}
"#;

#[test]
fn a_patch_for_a_missing_row_is_a_silent_no_op() {
    let dir = write_project(&[
        ("events/u.hk", RENAME_EVENTS),
        ("projectors/people.hk", RENAME_PROJECTOR),
    ]);
    let project = load_ok(dir.path());
    let projector = &project.projectors[0];
    let ModuleDef::Projector { entities, .. } = &projector.def else {
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
            event_type,
            json!({ "id": id, "name": name }),
        );
    }

    let model_dir = tempfile::tempdir().unwrap();
    let model = ReadModel::open(&model_dir.path().join("people.db"), entities).unwrap();
    let seen = project_to_head(&store, projector, &project.program, None, &model).unwrap();
    assert_eq!(seen, 3);

    let entity = entities.iter().find(|e| e.name == "Person").unwrap();
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
        ("events/u.hk", RENAME_EVENTS),
        ("projectors/people.hk", RENAME_PROJECTOR),
    ]);
    let project = load_ok(dir.path());
    let ModuleDef::Projector { entities, .. } = &project.projectors[0].def else {
        panic!("expected a projector");
    };
    let entity = entities.iter().find(|e| e.name == "Person").unwrap();

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
event @thing.added { id: Uuid, kind: String @max(20) }
event @thing.removed { id: Uuid, kind: String @max(20) }
event @thing.touched { id: Uuid, kind: String @max(20) }
"#;

/// The keys are the subscription, so `thing.touched` is never read. Two clauses name
/// `thing.added`: the constrained one selects a subset, and both run for an event that
/// matches both.
const PER_TYPE_PROJECTOR: &str = r#"
projector Things {
  entity Thing { id: Uuid @key, kind: String @max(20) }
  entity Special { id: Uuid @key, kind: String @max(20) }

  // Two handlers on one path, run in declaration order. A dispatch key cannot filter
  // any more, so what used to be `added(kind = "vip")` is an ordinary branch, and the
  // property under test (every selecting handler runs, in order) is unchanged.
  on @thing.added { id, kind } { put Thing { id, kind } }
  on @thing.added { id, kind } {
    if kind == "vip" {
      put Special { id, kind }
    }
  }
  on @thing.removed { id } { delete Thing[id] }
}
"#;

#[test]
fn a_clause_keyed_projector_handle_fans_out_and_subscribes_to_its_arms() {
    let dir = write_project(&[
        ("events/thing.hk", PER_TYPE_EVENTS),
        ("projectors/things.hk", PER_TYPE_PROJECTOR),
    ]);
    let project = load_ok(dir.path());
    let projector = &project.projectors[0];
    let ModuleDef::Projector { entities, .. } = &projector.def else {
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
            event_type,
            json!({ "id": id, "kind": kind }),
        );
    }

    let model_dir = tempfile::tempdir().unwrap();
    let model = ReadModel::open(&model_dir.path().join("things.db"), entities).unwrap();
    let seen = project_to_head(&store, projector, &project.program, None, &model).unwrap();
    assert_eq!(seen, 3, "the unsubscribed type is never read");

    let things = entities.iter().find(|e| e.name == "Thing").unwrap();
    let special = entities.iter().find(|e| e.name == "Special").unwrap();
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
event @thing.happened { id: Uuid }
"#;

const ID_PROJECTOR: &str = r#"
projector Things {
  entity Thing {
    id: Uuid @key,
    event_id: Uuid,
    derived: Uuid,
    at: Timestamp,
  }

  on @thing.happened as e { id } {
    put Thing {
      id,
      event_id: e.id,
      derived: Uuid.derive(e.id, "line-item"),
      at: e.at,
    }
  }
}
"#;

/// `event.id` and `event.timestamp` are the envelope's, and they do not move:
/// rebuilding a read model from position 0 has to reproduce both, or an id derived from
/// the first would change under a replay and a column holding the second would drift
/// from what the log says happened.
#[test]
fn the_envelope_fields_are_readable_and_survive_a_rebuild() {
    let dir = write_project(&[
        ("events/thing.hk", ID_EVENTS),
        ("projectors/things.hk", ID_PROJECTOR),
    ]);
    let project = load_ok(dir.path());
    let projector = &project.projectors[0];
    let ModuleDef::Projector { entities, .. } = &projector.def else {
        panic!("expected a projector");
    };

    let store_dir = tempfile::tempdir().unwrap();
    let (coordinator, store) = open_store(store_dir.path());
    seed_event(
        &store,
        &project,
        &ctx(),
        "thing.happened",
        json!({ "id": UUID_A }),
    );

    let project_once = || {
        let model_dir = tempfile::tempdir().unwrap();
        let model = ReadModel::open(&model_dir.path().join("things.db"), entities).unwrap();
        project_to_head(&store, projector, &project.program, None, &model).unwrap();
        let things = entities.iter().find(|e| e.name == "Thing").unwrap();
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

    // `seed_event` stamps the envelope with the shared test clock, so the column holds
    // the append time rather than a value the command restated in its payload.
    assert_eq!(row["at"], json!(TEST_NOW));

    // The whole point: a fresh model built from the same log lands on the same values.
    assert_eq!(project_once(), row);
    coordinator.shutdown();
}
