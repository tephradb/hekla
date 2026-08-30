//! An effect's `state` and what it is scoped by.
//!
//! `docs/effects.md` rules 2 and 3: state lives inside the arm, and the fold stops at
//! the trigger's own position, inclusive. Each case runs a real effect through
//! `hek test`, so what is asserted is what the driver would do.
//!
//! Two of the Starlark suite's cases are gone rather than ported. One checked that an
//! arm could not mutate the state it was handed; heklang has no mutable binding, so
//! there is nothing to attempt. The other checked that a subscription key could filter
//! on a subject-encrypted field in `query` and `fold` but not in a `handle` key;
//! heklang rejects an equality on sealed content anywhere (rule 12), and a handler is
//! selected by event path alone, so neither half of that distinction exists.

use std::process::ExitCode;

use hekla::testing;
use tempfile::TempDir;

mod support;

use support::write_project;

const A: &str = "11111111-1111-1111-1111-111111111111";
const B: &str = "22222222-2222-2222-2222-222222222222";

const EVENTS: &str = r#"
event @t.placed { id: Uuid, shop: Int }
event @t.noted { id: Uuid, shop: Int }
"#;

/// A project whose effect body and tests the caller supplies. `Record` is the sink an
/// effect reports through, so a case can assert folded state with `expect invoke`
/// rather than needing a read path.
fn project(effect: &str, scenario: &str) -> TempDir {
    write_project(&[
        ("events/t.hk", EVENTS),
        (
            "commands/record.hk",
            r#"
command Record(id: Uuid, shop: Int) {
  emit @t.noted { id, shop }
}
"#,
        ),
        ("effects/probe.hk", effect),
        ("tests/scenario.hk", scenario),
    ])
}

fn run(effect: &str, scenario: &str) -> String {
    format!("{:?}", testing::run(project(effect, scenario).path()))
}

fn ok() -> String {
    format!("{:?}", ExitCode::SUCCESS)
}

fn failed() -> String {
    format!("{:?}", ExitCode::FAILURE)
}

/// The boundary is scoped by the triggering event, so a `state` slice filters on a
/// field the trigger bound and the arm decides on what the fold produced.
#[test]
fn an_effect_folds_its_boundary_and_its_arm_sees_the_state() {
    let effect = r#"
effect Probe {
  on @t.placed { id, shop } {
    state count: Int = fold 0
      on @t.placed(shop) => count + 1

    invoke Record { id, shop: count }
  }
}
"#;
    // One event per shop. The boundary is scoped to the triggering event's shop, so
    // the second counts only itself rather than continuing from the first.
    let scenario = format!(
        r#"
test "the fold is scoped to the triggering event" {{
  given @t.placed {{ id: "{A}", shop: 1 }}
  given @t.placed {{ id: "{B}", shop: 2 }}
  deliver Probe
  expect invoke Record {{ id: "{A}", shop: 1 }}
  expect invoke Record {{ id: "{B}", shop: 1 }}
}}
"#
    );
    assert_eq!(run(effect, &scenario), ok());
}

/// The fold runs over `log[0..=N]`, so an effect that folds its own trigger type counts
/// itself. This is the semantics chosen (state is "the log at my position"), so it is
/// pinned rather than left to fall out of the implementation.
#[test]
fn the_fold_is_inclusive_of_the_triggering_event() {
    let effect = r#"
effect Probe {
  on @t.placed { id, shop } {
    state count: Int = fold 0
      on @t.placed(shop) => count + 1

    invoke Record { id, shop: count }
  }
}
"#;
    // Two events in one shop: the first sees 1 and the second sees 2, because each
    // counts itself.
    let scenario = format!(
        r#"
test "the trigger counts itself" {{
  given @t.placed {{ id: "{A}", shop: 1 }}
  given @t.placed {{ id: "{B}", shop: 1 }}
  deliver Probe
  expect invoke Record {{ id: "{A}", shop: 1 }}
  expect invoke Record {{ id: "{B}", shop: 2 }}
}}
"#
    );
    assert_eq!(run(effect, &scenario), ok());

    // And the other way round, so the assertion above is not passing by accident.
    let wrong = format!(
        r#"
test "the trigger counts itself" {{
  given @t.placed {{ id: "{A}", shop: 1 }}
  given @t.placed {{ id: "{B}", shop: 1 }}
  deliver Probe
  expect invoke Record {{ id: "{A}", shop: 0 }}
  expect invoke Record {{ id: "{B}", shop: 1 }}
}}
"#
    );
    assert_eq!(run(effect, &wrong), failed());
}

/// An arm with no `state` reads nothing, and its seed is whatever it declares.
#[test]
fn an_effect_without_a_boundary_uses_its_seed() {
    let effect = r#"
effect Probe {
  on @t.placed { id } {
    state count: Int = fold 7

    invoke Record { id, shop: count }
  }
}
"#;
    let scenario = format!(
        r#"
test "an unfolded state is its seed" {{
  given @t.placed {{ id: "{A}", shop: 1 }}
  deliver Probe
  expect invoke Record {{ id: "{A}", shop: 7 }}
}}
"#
    );
    assert_eq!(run(effect, &scenario), ok());
}

/// Two arms cannot select one event: rule 1 makes an event pick exactly one. What the
/// Starlark suite asserted with two arms sharing a clause is now a property of the
/// dispatch, and the checker is what says so.
#[test]
fn one_event_selects_exactly_one_arm() {
    let effect = r#"
effect Probe {
  on @t.placed { id } {
    invoke Record { id, shop: 1 }
  }

  on @t.placed { id } {
    invoke Record { id, shop: 2 }
  }
}
"#;
    let scenario = format!(
        r#"
test "unreachable" {{
  given @t.placed {{ id: "{A}", shop: 1 }}
  deliver Probe
  expect nothing
}}
"#
    );
    assert_eq!(run(effect, &scenario), failed());
}
