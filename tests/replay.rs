//! Projector replay: rebuild-and-swap reconstructs the read model from position 0
//! and keeps serving. Registers users, replays, then confirms the swapped-in model
//! holds every row, includes an event appended around the replay, and reports the
//! head position. If the rename or WAL handling were wrong, these reads would fail
//! or come back empty.

use std::path::Path;
use std::thread;
use std::time::Duration;

use kiln::context::CommandContext;
use kiln::loader::LoadedProject;
use kiln::read_api;
use kiln::runtime::Runtime;
use serde_json::json;
use uuid::Uuid;

const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
const C: &str = "cccccccc-cccc-cccc-cccc-cccccccccccc";

fn register(rt: &Runtime, user_id: &str) {
    let ctx = CommandContext::new(Uuid::new_v4());
    let body = json!({ "user_id": user_id, "email": format!("{user_id}@x"), "name": "U" });
    assert_eq!(
        rt.execute("register-user", body, &ctx, None)
            .unwrap()
            .status,
        200
    );
}

fn wait_position(rt: &Runtime, projector: &str, target: u64) {
    for _ in 0..300 {
        if rt.projector(projector).unwrap().position() >= target {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("projector `{projector}` did not reach position {target}");
}

/// The `users` row for `user_id`, read through the read API against the live file.
fn read_user(rt: &Runtime, user_id: &str) -> Option<serde_json::Value> {
    let shared = rt.projector("users").unwrap();
    let entity = read_api::find_entity(&shared.entities, "users").unwrap();
    read_api::get_one(&shared.db_path, entity, user_id)
        .unwrap()
        .0
}

#[test]
fn replay_rebuilds_and_keeps_serving() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/users");
    let project = LoadedProject::load(&root);
    assert!(!project.has_errors(), "{:?}", project.findings);
    let data = tempfile::tempdir().unwrap();
    let (rt, coord, projectors) = Runtime::open(project, data.path()).unwrap();

    register(&rt, A);
    register(&rt, B);
    wait_position(&rt, "users", 2);
    assert!(read_user(&rt, A).is_some());
    assert!(read_user(&rt, B).is_some());

    // Rebuild-and-swap both projectors from scratch, then append one more event
    // around the replay so the rebuild's catch-up must include it too.
    rt.projector("users").unwrap().request_replay();
    rt.projector("user-stats").unwrap().request_replay();
    register(&rt, C);

    // The rebuild projects to the current head (3), so the position returns to it.
    wait_position(&rt, "users", 3);
    wait_position(&rt, "user-stats", 3);

    // Every row survived the swap, and the new one is present.
    let alice = read_user(&rt, A).expect("A survived the rebuild");
    assert_eq!(alice["user_id"], A);
    assert!(read_user(&rt, B).is_some());
    assert!(read_user(&rt, C).is_some());

    // The running-total projector rebuilt to the right count, and the read reports
    // the rebuilt checkpoint position.
    let stats = rt.projector("user-stats").unwrap();
    let totals = read_api::find_entity(&stats.entities, "totals").unwrap();
    let (row, position) = read_api::get_one(&stats.db_path, totals, "all").unwrap();
    assert_eq!(row.unwrap()["count"].as_i64(), Some(3));
    assert_eq!(position, 3);

    projectors.shutdown_and_join();
    coord.shutdown();
}
