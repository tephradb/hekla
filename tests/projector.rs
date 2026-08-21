//! A projector reads through the event envelope: the shared `envelope::decode`
//! must unwrap `.data` in `project_to_head` (not only in the command fold), or a
//! projector would see the metadata wrapper instead of the payload.

use std::fs;
use std::path::Path;

use kiln::context::CommandContext;
use kiln::dispatch::build_event;
use kiln::loader::LoadedProject;
use kiln::projector::project_to_head;
use kiln::read_model::ReadModel;
use kiln::starlark_builtins::{EmittedEvent, ModuleDef};
use serde_json::json;
use tephra::{SegmentConfig, SegmentSet, WriteCoordinator, WriterConfig};
use uuid::Uuid;

const ALICE: &str = "11111111-1111-1111-1111-111111111111";

#[test]
fn projector_reads_through_the_envelope() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/users");
    let project = LoadedProject::load(&root);
    assert!(!project.has_errors(), "{:?}", project.findings);

    let projector = project
        .projectors
        .iter()
        .find(|unit| unit.loaded.def.name() == "users")
        .expect("users projector");
    let ModuleDef::Projector { entities, .. } = &projector.loaded.def else {
        panic!("expected a projector");
    };

    let store_dir = tempfile::tempdir().unwrap();
    let set = SegmentSet::open(
        store_dir.path().join("events"),
        SegmentConfig::new(16 * 1024 * 1024),
    )
    .unwrap();
    let (coordinator, store) = WriteCoordinator::start(set, WriterConfig::default()).unwrap();

    // Append a real, envelope-wrapped event through the same seam a command uses.
    let ctx = CommandContext::new(Uuid::new_v4());
    let emitted = EmittedEvent {
        event_type: "user.registered".to_owned(),
        data: json!({ "user_id": ALICE, "email": "alice@example.com", "name": "Alice" }),
        tags: vec![
            ("user_id".to_owned(), Some(ALICE.to_owned())),
            ("email".to_owned(), Some("alice@example.com".to_owned())),
        ],
    };
    let event = build_event(&emitted, &ctx, "1970-01-01T00:00:00Z").unwrap();
    store.append(vec![event], None).unwrap();

    let model_dir = tempfile::tempdir().unwrap();
    let model = ReadModel::open(&model_dir.path().join("users.db"), entities).unwrap();
    let seen = project_to_head(&store, &projector.loaded, &model).unwrap();
    assert_eq!(seen, 1);
    // The checkpoint advanced to the appended event.
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

fn write_file(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

#[test]
fn get_reads_through_uncommitted_writes_in_a_batch() {
    // A projector that keeps a running count with get()+put(): the second event in
    // a batch must observe the first event's still-uncommitted write, or the total
    // would land at 1 instead of 2.
    let dir = tempfile::tempdir().unwrap();
    write_file(
        dir.path(),
        "events/thing.star",
        r#"
happened = event(
    type = "thing.happened",
    fields = {"id": uuid()},
    tags = ["id"],
)
"#,
    );
    write_file(
        dir.path(),
        "projectors/counter.star",
        r#"
load("events/thing.star", "happened")

totals = entity(key = "id", fields = {"id": text(), "count": i64_()})

source = events(types = ["thing.happened"])

def handle(event):
    row = get(totals, "all")
    count = (row["count"] if row else 0) + 1
    return [put(totals, {"id": "all", "count": count})]
"#,
    );

    let project = LoadedProject::load(dir.path());
    assert!(!project.has_errors(), "{:?}", project.findings);
    let projector = &project.projectors[0];
    let ModuleDef::Projector { entities, .. } = &projector.loaded.def else {
        panic!("expected a projector");
    };

    let store_dir = tempfile::tempdir().unwrap();
    let set = SegmentSet::open(
        store_dir.path().join("events"),
        SegmentConfig::new(16 * 1024 * 1024),
    )
    .unwrap();
    let (coordinator, store) = WriteCoordinator::start(set, WriterConfig::default()).unwrap();

    let ctx = CommandContext::new(Uuid::new_v4());
    for _ in 0..2 {
        let id = Uuid::new_v4().to_string();
        let emitted = EmittedEvent {
            event_type: "thing.happened".to_owned(),
            data: json!({ "id": id }),
            tags: vec![("id".to_owned(), Some(id.clone()))],
        };
        let event = build_event(&emitted, &ctx, "1970-01-01T00:00:00Z").unwrap();
        store.append(vec![event], None).unwrap();
    }

    let model_dir = tempfile::tempdir().unwrap();
    let model = ReadModel::open(&model_dir.path().join("counter.db"), entities).unwrap();
    let seen = project_to_head(&store, &projector.loaded, &model).unwrap();
    assert_eq!(seen, 2);

    let entity = entities.iter().find(|e| e.name == "totals").unwrap();
    let row = model.get(entity, "all").unwrap().unwrap();
    assert_eq!(row["count"].as_i64(), Some(2));
    coordinator.shutdown();
}
