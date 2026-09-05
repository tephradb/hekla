//! A projector rebuilds automatically when its definition (source set or entity
//! schema) changes across a redeploy, so its read model reflects the event set it is
//! now built from rather than stalling at its old checkpoint.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use hekla::projector::Readiness;
use hekla::read_model::ReadModel;
use hekla::runtime::Runtime;
use rusqlite::Connection;
use serde_json::{Value, json};
use uuid::Uuid;

mod support;

use support::{Boot, Harness, ctx, get, send, wait_until};

const EVENTS: &str = r#"
event @e.one { id: Uuid }
event @e.two { id: Uuid }
"#;

const EMIT_ONE: &str = r#"
command EmitOne(id: Uuid) {
  emit @e.one { id }
}
"#;

const EMIT_TWO: &str = r#"
command EmitTwo(id: Uuid) {
  emit @e.two { id }
}
"#;

/// `hekla.toml` turning the automatic rebuild off.
const AUTO_REBUILD_OFF: &str = "[projectors]\nauto_rebuild = false\n";

/// One handler per event path, all doing the same thing: the shape the parameterised
/// fixtures below vary.
fn arms(paths: &str, body: &str) -> String {
    paths
        .split(", ")
        .map(|path| format!("  on {path} {{ {body} }}\n"))
        .collect()
}

/// The counter projector, parameterised by which paths it subscribes to.
fn counter(paths: &str) -> String {
    let arms = arms(paths, "patch Totals[\"all\"] { n: .n + 1 }");
    format!(
        r#"
projector Counter {{
  entity Totals {{ id: String @key @max(16), n: Int }}

{arms}}}
"#
    )
}

/// The same projector with an extra `label` column, so a redeploy changes the entity
/// schema rather than the subscription. `ReadModel::open` creates tables with
/// `IF NOT EXISTS`, so the column only appears if a rebuild ran first.
fn labelled_counter(paths: &str) -> String {
    let arms = arms(paths, "patch Totals[\"all\"] { n: .n + 1, label: \"x\" }");
    format!(
        r#"
projector Counter {{
  entity Totals {{ id: String @key @max(16), n: Int, label: String @max(8) }}

{arms}}}
"#
    )
}

/// Rewrite the project in place, so the next boot sees the redeployed definition.
fn write_project(dir: &Path, clauses: &str) {
    write_project_with(dir, counter(clauses), None);
}

/// The same, with an explicit projector module and an optional `hekla.toml`. Passing
/// `None` removes any config a previous deploy left, so a redeploy cannot inherit it.
fn write_project_with(dir: &Path, projector: String, config: Option<&str>) {
    for (rel, content) in [
        ("events/e.hk", EVENTS.to_owned()),
        ("commands/emit-one.hk", EMIT_ONE.to_owned()),
        ("commands/emit-two.hk", EMIT_TWO.to_owned()),
        ("projectors/counter.hk", projector),
    ] {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let config_path = dir.join("hekla.toml");
    match config {
        Some(text) => fs::write(config_path, text).unwrap(),
        None => drop(fs::remove_file(config_path)),
    }
}

fn boot(project_dir: &Path, data_dir: &Path) -> Harness {
    Boot::new(project_dir)
        .data_dir(data_dir)
        .http_status(200)
        .start()
}

fn emit(rt: &Runtime, command: &str) {
    let result = rt
        .execute(
            command,
            json!({ "id": Uuid::new_v4().to_string() }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 200, "{command}: {:?}", result.body);
}

/// Poll the `totals` row until `want` accepts it, giving up after three seconds.
fn wait_row(rt: &Runtime, want: impl Fn(&Value) -> bool) -> bool {
    for _ in 0..300 {
        let shared = rt.projector("Counter").unwrap();
        // The rebuild swaps a freshly built database in under the reader, so an open
        // can transiently fail; retry rather than panic on the race this test drives.
        let Ok(model) = ReadModel::open_readonly(&shared.db_path) else {
            thread::sleep(Duration::from_millis(10));
            continue;
        };
        let entity = shared.entities.iter().find(|e| e.name == "Totals").unwrap();
        if let Ok(Some(row)) = model.get(entity, "all")
            && want(&row)
        {
            return true;
        }
        drop(model);
        thread::sleep(Duration::from_millis(10));
    }
    false
}

fn wait_count(rt: &Runtime, expected: i64) -> bool {
    wait_row(rt, |row| row["n"].as_i64() == Some(expected))
}

/// Whatever the projector left on disk, named so a test can inspect it between boots.
fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join("projectors").join("Counter.db")
}

/// The definition hash recorded on the read model, read straight off the closed file.
fn recorded_definition(data_dir: &Path) -> Option<String> {
    ReadModel::open_readonly(&db_path(data_dir))
        .unwrap()
        .read_definition()
        .unwrap()
}

/// Put a directory where the rebuild's scratch database must go. `rebuild` clears any
/// leftover `*.rebuild.db` first, and removing a file fails on a directory, so the
/// rebuild stops at its first step with the live model untouched.
fn block_rebuild(data_dir: &Path) -> PathBuf {
    let blocker = db_path(data_dir).with_extension("rebuild.db");
    fs::create_dir_all(&blocker).unwrap();
    blocker
}

/// Why the projector is not advancing, for an assertion message.
fn diagnosis(rt: &Runtime) -> String {
    let shared = rt.projector("Counter").unwrap();
    format!(
        "readiness={} failed={} last_error={:?}",
        shared.readiness().label(),
        shared.failed(),
        shared.last_error()
    )
}

#[test]
fn a_source_set_change_rebuilds_the_projector() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    // Deploy A: the projector counts only `e.one`.
    write_project(project.path(), "@e.one");
    let a = boot(project.path(), data.path());
    for _ in 0..2 {
        emit(&a.rt, "EmitOne");
        emit(&a.rt, "EmitTwo");
    }
    assert!(wait_count(&a.rt, 2), "should count the two e.one events");
    a.shutdown();

    // Deploy B on the same data: the projector now also sources `e.two`. Its source
    // set changed, so it rebuilds from position 0 and counts all four events, rather
    // than resuming past the e.two events already in the log.
    write_project(project.path(), "@e.one, @e.two");
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

    write_project(project.path(), "@e.one");
    let a = boot(project.path(), data.path());
    emit(&a.rt, "EmitOne");
    assert!(wait_count(&a.rt, 1));
    a.shutdown();

    // Redeploy the identical project: no definition change, so the projector resumes
    // from its checkpoint (a rebuild would still land n=1, so also emit once more to
    // show it is advancing normally, not reprocessing from 0).
    let b = boot(project.path(), data.path());
    emit(&b.rt, "EmitOne");
    assert!(
        wait_count(&b.rt, 2),
        "should resume and count the new event"
    );
    b.shutdown();
}

/// The capability the digest reclaims.
///
/// The definition used to be a hand-rolled hash of the subscription and the entity
/// shapes, with the handler bodies deliberately left out: including them meant hashing
/// source text, and then every comment forced a full replay. So a corrected handler
/// changed nothing, and the model kept serving rows the old logic had built while
/// applying the new logic to everything after the checkpoint. The digest hashes what
/// runs, so this now rebuilds.
#[test]
fn a_handler_body_edit_rebuilds() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    let counting = |body: &str| {
        format!(
            "\nprojector Counter {{\n  entity Totals {{ id: String @key @max(16), n: Int }}\n\n\
             {}}}\n",
            arms("@e.one", body)
        )
    };

    write_project_with(
        project.path(),
        counting("patch Totals[\"all\"] { n: .n + 1 }"),
        None,
    );
    let a = boot(project.path(), data.path());
    emit(&a.rt, "EmitOne");
    emit(&a.rt, "EmitOne");
    assert!(wait_count(&a.rt, 2));
    let stamped = recorded_definition(data.path());
    a.shutdown();

    // Same subscription, same entity, different arithmetic. Nothing the old definition
    // hash could see moved.
    write_project_with(
        project.path(),
        counting("patch Totals[\"all\"] { n: .n + 2 }"),
        None,
    );
    let b = boot(project.path(), data.path());
    assert!(
        wait_count(&b.rt, 4),
        "the two recorded events must be re-folded by the corrected handler, not left \
         at what the old one wrote"
    );
    assert_ne!(
        recorded_definition(data.path()),
        stamped,
        "and the rebuild must stamp the new definition"
    );
    b.shutdown();
}

/// The other half of the same claim, and the one every scheme this replaced got wrong:
/// layout is not behaviour, so reformatting a projector must not cost a full replay.
#[test]
fn a_cosmetic_edit_does_not_rebuild() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    write_project(project.path(), "@e.one");
    let a = boot(project.path(), data.path());
    emit(&a.rt, "EmitOne");
    assert!(wait_count(&a.rt, 1));
    let stamped = recorded_definition(data.path());
    a.shutdown();

    // Reindented, commented, and with the handler's binding renamed. Every byte of the
    // file moved; nothing it does did.
    let reformatted = r#"
// The running total, across every @e.one ever appended.
projector Counter {
  entity Totals {
      id:    String @key @max(16),
      n:     Int
  }

  on @e.one as appended {

    // One more.
    patch Totals["all"] { n: .n + 1 }
  }
}
"#;
    write_project_with(project.path(), reformatted.to_owned(), None);
    let b = boot(project.path(), data.path());
    assert_eq!(
        recorded_definition(data.path()),
        stamped,
        "a reformat is not a definition change"
    );
    emit(&b.rt, "EmitOne");
    assert!(
        wait_count(&b.rt, 2),
        "so the projector resumes from its checkpoint rather than replaying from 0"
    );
    b.shutdown();
}

#[test]
fn an_added_entity_field_rebuilds_with_the_new_column() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    write_project(project.path(), "@e.one");
    let a = boot(project.path(), data.path());
    emit(&a.rt, "EmitOne");
    emit(&a.rt, "EmitOne");
    assert!(wait_count(&a.rt, 2));
    a.shutdown();

    // Deploy B keeps the same source set but grows the entity by one column. The
    // rebuild has to run before the first batch: an `INSERT` naming `label` against
    // the old table would fail on the missing column and wedge the thread for good.
    write_project_with(project.path(), labelled_counter("@e.one"), None);
    let b = boot(project.path(), data.path());
    assert!(
        wait_row(&b.rt, |row| row["n"].as_i64() == Some(2)
            && row["label"] == "x"),
        "the rebuilt table should carry the new column: {}",
        diagnosis(&b.rt)
    );
    assert!(
        !b.rt.projector("Counter").unwrap().failed(),
        "the projector should not have died applying the new shape"
    );
    b.shutdown();
}

#[test]
fn auto_rebuild_off_idles_stale_and_leaves_the_definition_unstamped() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    write_project_with(project.path(), counter("@e.one"), Some(AUTO_REBUILD_OFF));
    let a = boot(project.path(), data.path());
    for _ in 0..2 {
        emit(&a.rt, "EmitOne");
        emit(&a.rt, "EmitTwo");
    }
    assert!(wait_count(&a.rt, 2));
    a.shutdown();
    let stamped_by_a = recorded_definition(data.path()).expect("deploy A recorded its definition");

    // Deploy B changes the source set with auto-rebuild off. The model on disk was
    // built from a different event set, so the projector reports `stale` and idles
    // rather than applying batches on top of it.
    write_project_with(
        project.path(),
        counter("@e.one, @e.two"),
        Some(AUTO_REBUILD_OFF),
    );
    let b = boot(project.path(), data.path());
    assert_eq!(
        b.rt.projector("Counter").unwrap().readiness(),
        Readiness::Stale
    );
    emit(&b.rt, "EmitTwo");
    assert!(
        !wait_count(&b.rt, 3),
        "a stale projector must not apply batches onto the old model: {}",
        diagnosis(&b.rt)
    );
    assert!(
        wait_count(&b.rt, 2),
        "the old model is left exactly as it was"
    );
    b.shutdown();

    // The crux: deploy B must not stamp its own definition onto a model it did not
    // rebuild. If it had, the mismatch would be invisible forever after.
    assert_eq!(
        recorded_definition(data.path()),
        Some(stamped_by_a),
        "an unrebuilt model keeps the definition it was actually built under"
    );

    // Deploy C is deploy B with auto-rebuild back on. The mismatch is still there to
    // be found, so it rebuilds and counts all five events (2x e.one, 3x e.two).
    write_project_with(project.path(), counter("@e.one, @e.two"), None);
    let c = boot(project.path(), data.path());
    assert!(
        wait_count(&c.rt, 5),
        "the still-visible mismatch should rebuild from position 0: {}",
        diagnosis(&c.rt)
    );
    c.shutdown();
}

#[test]
fn a_legacy_read_model_without_a_definition_hash_is_rebuilt_not_blessed() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    write_project(project.path(), "@e.one");
    let a = boot(project.path(), data.path());
    emit(&a.rt, "EmitOne");
    emit(&a.rt, "EmitOne");
    assert!(wait_count(&a.rt, 2));
    a.shutdown();

    // Age the model into one written before the definition hash existed, and put a
    // wrong count in it so "rebuilt" and "resumed" are distinguishable: a rebuild
    // from position 0 recomputes n=2, while blessing the file leaves n=99.
    let conn = Connection::open(db_path(data.path())).unwrap();
    conn.execute("UPDATE _hekla_definition SET definition_hash = NULL", [])
        .unwrap();
    conn.execute("UPDATE totals SET n = 99", []).unwrap();
    drop(conn);

    // With auto-rebuild off, an unverifiable shape is `stale`, not `ready`: the model
    // is left untouched and its hash stays NULL so the next boot can still act on it.
    write_project_with(project.path(), counter("@e.one"), Some(AUTO_REBUILD_OFF));
    let b = boot(project.path(), data.path());
    assert_eq!(
        b.rt.projector("Counter").unwrap().readiness(),
        Readiness::Stale
    );
    assert!(wait_count(&b.rt, 99), "the legacy model is not touched");
    b.shutdown();
    assert_eq!(
        recorded_definition(data.path()),
        None,
        "stamping a model that was never verified would silence this forever"
    );

    // With auto-rebuild on, the same unverifiable model is rebuilt from position 0
    // and only then records the definition it was built under.
    write_project(project.path(), "@e.one");
    let c = boot(project.path(), data.path());
    assert!(
        wait_count(&c.rt, 2),
        "the rebuild should discard the planted count: {}",
        diagnosis(&c.rt)
    );
    c.shutdown();
    assert!(
        recorded_definition(data.path()).is_some(),
        "a rebuilt model records its definition as the new baseline"
    );
}

#[test]
fn shutdown_drains_pending_events_to_head() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    write_project(project.path(), "@e.one");
    let a = boot(project.path(), data.path());
    // Deliberately no wait: the shutdown lands while the projector is still behind,
    // and the loop must drain to head before it exits rather than dropping the tail.
    for _ in 0..20 {
        emit(&a.rt, "EmitOne");
    }
    a.shutdown();

    let model = ReadModel::open_readonly(&db_path(data.path())).unwrap();
    assert_eq!(
        model.read_checkpoint().unwrap().get(),
        20,
        "the checkpoint should reach head, not stop where the flag was seen"
    );
    drop(model);

    // Resuming from that checkpoint counts every event exactly once: a lagging
    // checkpoint would replay the tail and push the running total past 21.
    let b = boot(project.path(), data.path());
    assert!(
        wait_count(&b.rt, 20),
        "the drained total survives the restart"
    );
    emit(&b.rt, "EmitOne");
    assert!(
        wait_count(&b.rt, 21),
        "each event is counted once across the restart: {}",
        diagnosis(&b.rt)
    );
    b.shutdown();
}

/// A rebuild that fails leaves the read model at the shape it had, so it cannot be
/// served or advanced. What it must not do is take the projector's thread down with
/// it: that made `POST /replay` a promise nothing would keep, and left the read API
/// answering `rebuilding` (retry, this resolves itself) forever. It idles instead,
/// says why, and a replay recovers it in place.
#[tokio::test]
async fn a_failed_rebuild_idles_and_a_replay_recovers_it() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    write_project(project.path(), "@e.one");
    let a = boot(project.path(), data.path());
    for _ in 0..2 {
        emit(&a.rt, "EmitOne");
        emit(&a.rt, "EmitTwo");
    }
    assert!(wait_count(&a.rt, 2));
    a.shutdown();

    // Deploy B changes the source set, so boot plans a rebuild; the blocker fails it.
    let blocker = block_rebuild(data.path());
    write_project(project.path(), "@e.one, @e.two");
    let b = boot(project.path(), data.path());
    let shared = Arc::clone(b.rt.projector("Counter").unwrap());
    wait_until("the rebuild to fail", || {
        shared.readiness() == Readiness::Failed
    });

    assert!(
        shared.running(),
        "a failed rebuild must not stop the thread"
    );
    assert!(shared.failed());
    assert!(shared.last_error().is_some(), "the failure names its cause");

    // The model is still the old shape, so it must not take batches built at the new
    // one, and it must not be served as though it could.
    emit(&b.rt, "EmitTwo");
    assert!(
        !wait_count(&b.rt, 3),
        "a projector that failed to rebuild must not apply batches: {}",
        diagnosis(&b.rt)
    );
    assert!(wait_count(&b.rt, 2), "the old model is left as it was");

    let app = b.app();
    let (status, body) = get(&app, "/read/Counter/Totals/all").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "rebuild_failed");

    // Clear the cause and retry: no restart, and the failure clears with it.
    fs::remove_dir_all(&blocker).unwrap();
    let (status, _) = send(&app, Method::POST, "/projectors/Counter/replay").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    wait_until("the replay to rebuild the model", || {
        shared.readiness() == Readiness::Ready
    });
    assert!(!shared.failed(), "recovering clears the recorded failure");
    assert!(shared.last_error().is_none());

    // Five events match the new source set: four from deploy A and the one above.
    assert!(
        wait_count(&b.rt, 5),
        "the rebuild reprocesses every matching event: {}",
        diagnosis(&b.rt)
    );
    let (status, body) = get(&app, "/read/Counter/Totals/all").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["item"]["n"].as_i64(), Some(5));

    b.shutdown();
}
