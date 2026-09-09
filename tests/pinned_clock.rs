//! One `test` declaration, two runners, one verdict.
//!
//! `hek test` and `hekla test` share heklang's runner and its `World` trait, so `expect`
//! means one thing. What they do not share is the **synthesised envelope**: the id and
//! the append time a world invents for a `given` event and for whatever the action
//! appends. Those are not facts about the world, they are what a runner made up, and a
//! world that makes them up differently gives one declaration two meanings.
//!
//! That is a bug an ordinary hekla test cannot find, because it needs a `.hk` case that
//! reads `e.at` or `now()` and is then run under *both* runners. Repo-wide, `e.at`
//! appears in one other `.hk` source, so until this file existed nothing checked it, and
//! the divergence surfaced downstream as thirteen failures nobody could place.
//!
//! Every case below therefore runs twice: through `hekla::testing::run`, which is
//! hekla's world, and through `heklang::run_tests`, which is heklang's own `Sandbox`.
//! Agreeing is the assertion.

use std::process::ExitCode;
use std::sync::Arc;

use hekla::heklang_host::{HeklaHost, Stamp, event_from_json};
use hekla::testing;
use heklang::Event;
use heklang::host::{AppendCondition, Log};
use serde_json::json;
use tempfile::TempDir;

mod support;

use support::{load_ok, write_project};

const EVENTS: &str = r#"
event @thing.happened { id: Uuid, note: String @max(120) }
event @thing.noted { id: Uuid, due_at: Timestamp }
"#;

/// `created_at: e.at` is the line the whole file is about: the envelope's append time,
/// read into a column, which is what a `created_at` is supposed to be built from.
const PROJECTOR: &str = r#"
projector Things {
  entity Thing {
    id: Uuid @key,
    note: String @max(120),
    event_id: Uuid,
    created_at: Timestamp,
  }

  on @thing.happened as e { id, note } {
    put Thing { id, note, event_id: e.id, created_at: e.at }
  }
}
"#;

/// One command, and it exists for `now()`: a `.hk` case can read the clock through a
/// payload the command wrote, which is the one route to it that does not go through a
/// projector. There is deliberately no multi-`emit` command here, because no case could
/// observe its stamps; `two_events_in_one_append_are_stamped_from_their_own_positions`
/// says why and covers it from Rust instead.
const COMMANDS: &str = r#"
command Schedule(id: Uuid) {
  emit @thing.noted { id, due_at: now() }
}
"#;

fn project(scenario: &str) -> TempDir {
    write_project(&[
        ("events/thing.hk", EVENTS),
        ("projectors/things.hk", PROJECTOR),
        ("commands/note.hk", COMMANDS),
        ("tests/scenario.hk", scenario),
    ])
}

/// hekla's world: a real tephra log, real SQLite read models, a real key store.
fn hekla(scenario: &str) -> String {
    format!("{:?}", testing::run(project(scenario).path()))
}

/// heklang's own world, over the same program text. This is the half that says what
/// the answer is *supposed* to be, so a case passing here and failing above is the
/// two-dialect drift, stated.
fn heklang(scenario: &str) -> String {
    let dir = project(scenario);
    // `load_ok` rather than a raw load, so both halves are fed a program that passed the
    // same gate: `testing::run` refuses a project with error findings, and this side
    // would otherwise run `run_tests` over a partial program and report a runner
    // disagreement that is really a broken fixture.
    let program = load_ok(dir.path()).program;
    let results = heklang::run_tests(&program);
    assert!(!results.is_empty(), "the scenario declared no tests");
    let failures: Vec<String> = results
        .iter()
        .filter_map(|result| match &result.outcome {
            heklang::TestOutcome::Passed => None,
            heklang::TestOutcome::Failed(why) => Some(format!("FAIL: {:?}: {why}", result.name)),
            heklang::TestOutcome::Errored(why) => Some(format!("ERROR: {:?}: {why}", result.name)),
        })
        .collect();
    if failures.is_empty() {
        ok()
    } else {
        failures.join("\n")
    }
}

fn ok() -> String {
    format!("{:?}", ExitCode::SUCCESS)
}

fn failed() -> String {
    format!("{:?}", ExitCode::FAILURE)
}

/// The reported bug, at its narrowest: one `given`, one column, one literal. This is
/// the case FlowWarranty had thirteen of, each reading `expected 1577836800000000,
/// got 0`.
#[test]
fn a_seeded_events_append_time_is_the_epoch_both_runners_start_from() {
    let scenario = r#"
test "the first given event is stamped at the harness epoch" {
  given @thing.happened { id: "11111111-1111-1111-1111-111111111111", note: "first" }
  project Things
  expect Thing["11111111-1111-1111-1111-111111111111"] {
    created_at: "2020-01-01T00:00:00Z",
  }
}
"#;
    assert_eq!(hekla(scenario), ok());
    assert_eq!(heklang(scenario), ok());
}

/// The half a constant swap would have missed. heklang's clock is derived from the
/// log's length, not frozen, so seeding two events puts a minute between them, and a
/// world that pinned one instant would agree about the first row and not the second.
#[test]
fn seeded_events_are_stamped_a_minute_apart() {
    let scenario = r#"
test "each given event is stamped from its own position" {
  given @thing.happened { id: "11111111-1111-1111-1111-111111111111", note: "first" }
  given @thing.happened { id: "22222222-2222-2222-2222-222222222222", note: "second" }
  project Things
  expect Thing["11111111-1111-1111-1111-111111111111"] {
    created_at: "2020-01-01T00:00:00Z",
  }
  expect Thing["22222222-2222-2222-2222-222222222222"] {
    created_at: "2020-01-01T00:01:00Z",
  }
}
"#;
    assert_eq!(hekla(scenario), ok());
    assert_eq!(heklang(scenario), ok());
}

/// `now()` reads the instant the *next* append is stamped with, which is what makes an
/// emitted event's `e.at` equal to the `now()` its own command saw. Asserted through a
/// command's payload rather than a column, so it is the `Clock` half under test and not
/// the envelope's.
#[test]
fn now_reads_the_instant_the_next_append_is_stamped_with() {
    let scenario = r#"
test "now() advances past the given log" {
  given @thing.happened { id: "11111111-1111-1111-1111-111111111111", note: "first" }
  given @thing.happened { id: "22222222-2222-2222-2222-222222222222", note: "second" }
  run Schedule { id: "33333333-3333-3333-3333-333333333333" }
  expect @thing.noted {
    id: "33333333-3333-3333-3333-333333333333",
    due_at: "2020-01-01T00:02:00Z",
  }
}
"#;
    assert_eq!(hekla(scenario), ok());
    assert_eq!(heklang(scenario), ok());
}

/// The other half of one append, which no `.hk` case can reach.
///
/// A test does one thing: heklang rejects a `run` and a `project` in the same case, and
/// an effect fires on `given` events rather than on a command's output. So the only
/// envelopes a `.hk` test can read are the seeded ones, one event per append, and the
/// stamps a *batch* gets are invisible from up there. They are still worth pinning: the
/// whole point of deriving both halves from the log position is that there is no seam
/// to remember, and this is the seam a per-invocation stamp would have been wrong at.
///
/// Note what it asserts. A live hekla append stamps one instant across the whole
/// request, and this says two events from one append are a minute apart. That is
/// heklang's harness, reproduced deliberately: the fiction is heklang's to define, and
/// hekla's job below the seam is to write down the same one.
#[test]
fn two_events_in_one_append_are_stamped_from_their_own_positions() {
    let source = project("");
    let loaded = load_ok(source.path());
    let data = tempfile::tempdir().unwrap();
    let (coordinator, store) = support::open_store(data.path());

    let mut host = HeklaHost {
        program: Arc::clone(&loaded.program),
        events: Arc::clone(&loaded.events),
        store: store.clone(),
        keystore: None,
        ctx: support::ctx(),
        stamp: Stamp::Pinned,
        idem_tag: None,
        call: None,
        appended: None,
        emitted: Vec::new(),
        unavailable: None,
        duplicated: false,
        http: None,
        secrets: None,
        retry_after: None,
        last_transport: None,
        sealed: false,
    };

    let events: Vec<Event> = ["first", "second", "third"]
        .into_iter()
        .map(|note| {
            event_from_json(
                &loaded.program,
                "thing.happened",
                &json!({ "id": support::ALICE, "note": note }),
            )
            .expect("a declared event")
        })
        .collect();
    Log::append(
        &mut host,
        &events,
        &AppendCondition {
            after: 0,
            slices: Vec::new(),
        },
    )
    .expect("the append lands");

    for position in 0..3u64 {
        let record = Log::record(&host, position)
            .expect("the log reads back")
            .expect("a record that was just appended");
        assert_eq!(
            record.id,
            format!("0190d1a1-0000-7000-9000-{position:012}"),
            "the id at position {position}"
        );
        assert_eq!(
            record.at,
            1_577_836_800_000_000 + position as i64 * 60_000_000,
            "the append time at position {position}"
        );
    }

    coordinator.shutdown();
}

/// The id half of the envelope, which diverged for the same reason and had bitten
/// nothing yet only because no shipped project asserts one.
///
/// `e.id` is asserted rather than a `Uuid.derive` over it, because the derivation is a
/// pure function of its seed: two runners that agree here cannot disagree there, and a
/// literal uuid5 in a fixture would only restate heklang's hash.
#[test]
fn a_seeded_events_id_is_the_same_under_both_runners() {
    let scenario = r#"
test "a given event carries the harness id for its position" {
  given @thing.happened { id: "11111111-1111-1111-1111-111111111111", note: "first" }
  given @thing.happened { id: "22222222-2222-2222-2222-222222222222", note: "second" }
  project Things
  expect Thing["11111111-1111-1111-1111-111111111111"] {
    event_id: "0190d1a1-0000-7000-9000-000000000000",
  }
  expect Thing["22222222-2222-2222-2222-222222222222"] {
    event_id: "0190d1a1-0000-7000-9000-000000000001",
  }
}
"#;
    assert_eq!(hekla(scenario), ok());
    assert_eq!(heklang(scenario), ok());
}

/// The negative, so the cases above are known to be able to fail. A world frozen at the
/// old constant would have passed this one.
#[test]
fn the_old_frozen_epoch_no_longer_passes() {
    let scenario = r#"
test "the epoch is not 1970" {
  given @thing.happened { id: "11111111-1111-1111-1111-111111111111", note: "first" }
  project Things
  expect Thing["11111111-1111-1111-1111-111111111111"] {
    created_at: "1970-01-01T00:00:00Z",
  }
}
"#;
    assert_eq!(hekla(scenario), failed());
    assert_ne!(heklang(scenario), ok());
}
