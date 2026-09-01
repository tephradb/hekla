//! Projector replay: rebuild-and-swap reconstructs the read model from position 0
//! and keeps serving. Registers users, replays, then confirms the swapped-in model
//! holds every row, includes an event appended around the replay, and reports the
//! head position. If the rename or WAL handling were wrong, these reads would fail
//! or come back empty.

use std::sync::Arc;

use hekla::read_api;
use hekla::read_model::ReadModel;
use hekla::runtime::Runtime;
use tephra::Position;

mod support;

use support::{
    UUID_A, UUID_B, UUID_C, boot_example, boot_example_at, register_user, wait_position,
};

fn register(rt: &Runtime, user_id: &str) {
    register_user(rt, user_id, &format!("{user_id}@x"), "U");
}

/// The `users` row for `user_id`, read through the read API against the live file.
fn read_user(rt: &Runtime, user_id: &str) -> Option<serde_json::Value> {
    let shared = rt.projector("Users").unwrap();
    let entity = read_api::find_entity(&shared.entities, "User").unwrap();
    read_api::get_one(&shared.db_path, entity, user_id, None)
        .unwrap()
        .0
}

#[test]
fn replay_rebuilds_and_keeps_serving() {
    let harness = boot_example();
    let rt = &harness.rt;

    register(rt, UUID_A);
    register(rt, UUID_B);
    wait_position(rt, "Users", 2);
    assert!(read_user(rt, UUID_A).is_some());
    assert!(read_user(rt, UUID_B).is_some());

    // Rebuild-and-swap both projectors from scratch, then append one more event
    // around the replay so the rebuild's catch-up must include it too.
    rt.projector("Users").unwrap().request_replay();
    rt.projector("UserStats").unwrap().request_replay();
    register(rt, UUID_C);

    // The rebuild projects to the current head (3), so the position returns to it.
    wait_position(rt, "Users", 3);
    wait_position(rt, "UserStats", 3);

    // Every row survived the swap, and the new one is present.
    let alice = read_user(rt, UUID_A).expect("A survived the rebuild");
    assert_eq!(alice["user_id"], UUID_A);
    assert!(read_user(rt, UUID_B).is_some());
    assert!(read_user(rt, UUID_C).is_some());

    // The running-total projector rebuilt to the right count, and the read reports
    // the rebuilt checkpoint position.
    let stats = rt.projector("UserStats").unwrap();
    let totals = read_api::find_entity(&stats.entities, "Totals").unwrap();
    let (row, position) = read_api::get_one(&stats.db_path, totals, "all", None).unwrap();
    assert_eq!(row.unwrap()["count"].as_i64(), Some(3));
    assert_eq!(position, 3);

    harness.shutdown();
}

#[test]
fn a_replay_discards_a_stale_rebuild_file() {
    // A caller-owned data directory, so the read model outlives the harness and can
    // be inspected once the projector thread has joined.
    let data = tempfile::tempdir().unwrap();
    let harness = boot_example_at(data.path());
    let rt = &harness.rt;

    register(rt, UUID_A);
    register(rt, UUID_B);
    wait_position(rt, "Users", 2);

    // A crash mid-rebuild leaves a partial sibling behind. Plant one that is a valid,
    // openable read model whose checkpoint already claims head but which holds no
    // rows: reusing it rather than deleting it would swap an empty model into place
    // and never notice, because there is nothing left for it to project.
    let shared = rt.projector("Users").unwrap();
    let db_path = shared.db_path.clone();
    let entities = Arc::clone(&shared.entities);
    let rebuild_path = db_path.with_extension("rebuild.db");
    let planted = ReadModel::open(&rebuild_path, &entities).unwrap();
    planted.advance_checkpoint(Position::new(3)).unwrap();
    drop(planted);

    register(rt, UUID_C);
    shared.request_replay();
    wait_position(rt, "Users", 3);

    // Shut down before inspecting: the loop takes a pending replay before it takes a
    // pending shutdown, so joining the thread means the rebuild has finished.
    harness.shutdown();

    let model = ReadModel::open_readonly(&db_path).unwrap();
    let entity = entities.iter().find(|e| e.name == "User").unwrap();
    for user_id in [UUID_A, UUID_B, UUID_C] {
        assert!(
            model.get(entity, user_id).unwrap().is_some(),
            "{user_id} is missing, so the replay reused the planted file"
        );
    }
    assert_eq!(model.read_checkpoint().unwrap().get(), 3);
    drop(model);
    assert!(
        !rebuild_path.exists(),
        "the rebuild sibling is renamed into place, never left behind"
    );
}

/// A rebuild happens into a sibling file and swaps in by rename, so the live position
/// never drops and nothing else on the handle says one occurred. The counters are the
/// only record, which is why they are on `/status` rather than test-only: an operator
/// asking "has this projector been replayed" has nowhere else to look.
#[test]
fn a_replay_is_counted_and_reported() {
    let harness = boot_example();
    register(&harness.rt, UUID_A);
    wait_position(&harness.rt, "Users", 1);

    let handle = harness.rt.projector("Users").unwrap();
    assert_eq!(handle.replays_completed(), 0, "nothing has replayed yet");
    assert_eq!(handle.replays_failed(), 0);

    support::replay_and_wait(&harness.rt, "Users");
    assert_eq!(handle.replays_completed(), 1);
    assert_eq!(handle.replays_failed(), 0, "the rebuild succeeded");

    support::replay_and_wait(&harness.rt, "Users");
    assert_eq!(handle.replays_completed(), 2, "the count is cumulative");

    // The same numbers an operator reads, from the same handle.
    let status = harness.rt.status();
    let reported = status["projectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "Users")
        .expect("the projector is in /status");
    assert_eq!(reported["replays_completed"], 2);
    assert_eq!(reported["replays_failed"], 0);
    // Untouched by its neighbour's replays, so the counter is per projector.
    let other = status["projectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "UserStats")
        .expect("the other projector is in /status");
    assert_eq!(other["replays_completed"], 0);

    harness.shutdown();
}
