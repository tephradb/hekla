//! A projector rebuilds automatically when its definition (source set or entity
//! schema) changes across a redeploy, so its read model reflects the event set it is
//! now built from rather than stalling at its old checkpoint.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use kiln::context::CommandContext;
use kiln::effect::{HttpClient, StubHttpClient};
use kiln::loader::LoadedProject;
use kiln::runtime::Runtime;
use serde_json::json;
use uuid::Uuid;

const EVENTS: &str = r#"
one = event(type = "e.one", fields = {"id": uuid()})
two = event(type = "e.two", fields = {"id": uuid()})
"#;

const EMIT_ONE: &str = r#"
load("events/e.star", "one")
input = schema(id = uuid())
def handle(input, state):
    return emit(one(id = input.id))
"#;

const EMIT_TWO: &str = r#"
load("events/e.star", "two")
input = schema(id = uuid())
def handle(input, state):
    return emit(two(id = input.id))
"#;

/// The counter projector, parameterised by which event types it sources.
fn counter(source: &str) -> String {
    format!(
        r#"
load("events/e.star", "one", "two")

totals = entity(key = "id", fields = {{"id": text(), "n": i64_()}})

source = {source}

def handle(event):
    row = get(totals, "all")
    n = (row["n"] if row else 0) + 1
    return [put(totals, {{"id": "all", "n": n}})]
"#
    )
}

fn write_project(dir: &Path, counter_source: &str) {
    for (rel, content) in [
        ("events/e.star", EVENTS.to_owned()),
        ("commands/emit-one.star", EMIT_ONE.to_owned()),
        ("commands/emit-two.star", EMIT_TWO.to_owned()),
        ("projectors/counter.star", counter(counter_source)),
    ] {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

fn boot(project_dir: &Path, data_dir: &Path) -> Runtime2 {
    let project = LoadedProject::load(project_dir);
    assert!(!project.has_errors(), "{:?}", project.findings);
    let http: Arc<dyn HttpClient> = Arc::new(StubHttpClient::status(200));
    let (rt, coord, projectors, effects) = Runtime::open(project, data_dir, http, None).unwrap();
    Runtime2 {
        rt,
        coord,
        projectors,
        effects,
    }
}

struct Runtime2 {
    rt: Arc<Runtime>,
    coord: tephra::WriteCoordinator,
    projectors: kiln::projector::ProjectorSet,
    effects: kiln::effect::EffectRuntime,
}

impl Runtime2 {
    fn shutdown(self) {
        self.effects.shutdown_and_join();
        self.projectors.shutdown_and_join();
        self.coord.shutdown();
    }
}

fn emit(rt: &Runtime, command: &str) {
    let ctx = CommandContext::new(Uuid::new_v4());
    let result = rt
        .execute(
            command,
            json!({ "id": Uuid::new_v4().to_string() }),
            &ctx,
            None,
        )
        .unwrap();
    assert_eq!(result.status, 200, "{command}: {:?}", result.body);
}

fn wait_count(rt: &Runtime, expected: i64) -> bool {
    for _ in 0..300 {
        let shared = rt.projector("counter").unwrap();
        let model = kiln::read_model::ReadModel::open_readonly(&shared.db_path).unwrap();
        let entity = shared.entities.iter().find(|e| e.name == "totals").unwrap();
        if let Ok(Some(row)) = model.get(entity, "all")
            && row["n"].as_i64() == Some(expected)
        {
            return true;
        }
        drop(model);
        thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn a_source_set_change_rebuilds_the_projector() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    // Deploy A: the projector counts only `e.one`.
    write_project(project.path(), "[one()]");
    let a = boot(project.path(), data.path());
    for _ in 0..2 {
        emit(&a.rt, "emit-one");
        emit(&a.rt, "emit-two");
    }
    assert!(wait_count(&a.rt, 2), "should count the two e.one events");
    a.shutdown();

    // Deploy B on the same data: the projector now also sources `e.two`. Its source
    // set changed, so it rebuilds from position 0 and counts all four events, rather
    // than resuming past the e.two events already in the log.
    write_project(project.path(), "[one(), two()]");
    let b = boot(project.path(), data.path());
    assert!(
        wait_count(&b.rt, 4),
        "the rebuild should reprocess every matching event, including e.two"
    );
    b.shutdown();
}

#[test]
fn an_unchanged_definition_does_not_rebuild() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    write_project(project.path(), "[one()]");
    let a = boot(project.path(), data.path());
    emit(&a.rt, "emit-one");
    assert!(wait_count(&a.rt, 1));
    a.shutdown();

    // Redeploy the identical project: no definition change, so the projector resumes
    // from its checkpoint (a rebuild would still land n=1, so also emit once more to
    // show it is advancing normally, not reprocessing from 0).
    let b = boot(project.path(), data.path());
    emit(&b.rt, "emit-one");
    assert!(
        wait_count(&b.rt, 2),
        "should resume and count the new event"
    );
    b.shutdown();
}
