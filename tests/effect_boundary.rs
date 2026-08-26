//! An effect's `query` / `initial` / `fold` boundary: the state a handler decides on.
//!
//! State reaches an effect the same way it reaches a command, by folding the event
//! log, with one difference that is the whole point of the design: the fold stops at
//! the effect's own position. That makes it a function of the log prefix and the
//! triggering event, so it is identical on every attempt and every replay, and it can
//! never observe a race with anything downstream.
//!
//! The semantics live here as `hekla test` scenarios, which run the real dispatch path.
//! The live-runtime properties (position bounding under lag, and the fold not touching
//! the journal) are in `tests/effect_journal.rs`, where the op-DB is readable.

use std::process::ExitCode;

use hekla::testing;
use tempfile::TempDir;

mod support;

use support::write_project;

const A: &str = "11111111-1111-1111-1111-111111111111";
const B: &str = "22222222-2222-2222-2222-222222222222";

const EVENTS: &str = r#"
placed = event(type = "t.placed", fields = {"id": uuid(), "shop": uint()})
noted = event(type = "t.noted", fields = {"id": uuid(), "shop": uint()})
"#;

/// A project whose effect body and scenario the caller supplies. The `record` command
/// is the sink an effect reports through, so a scenario can assert folded state with
/// `command_call` rather than needing a read path.
fn project(effect: &str, scenario: &str) -> TempDir {
    write_project(&[
        ("events/t.star", EVENTS),
        (
            "commands/record.star",
            r#"
load("events/t.star", "noted")

input = schema(id = uuid(), shop = uint())

def handle(input, state):
    return noted(id = input.id, shop = input.shop)
"#,
        ),
        ("effects/probe.star", effect),
        ("tests/scenario.star", scenario),
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

/// The boundary is scoped by the triggering event, so `query` takes it where a
/// command's takes `input`, and the arm decides on what the fold produced.
#[test]
fn an_effect_folds_its_boundary_and_its_arm_sees_the_state() {
    let effect = r#"
load("events/t.star", "placed")

def query(event):
    return [placed(shop = event.data.shop)]

initial = {"count": 0}

fold = {placed(): lambda state, event: {"count": state["count"] + 1}}

def probe(event, state):
    invoke_command("record", {"id": event.data.id, "shop": state["count"]})

handle = {placed(): probe}
"#;
    // One event per shop. The boundary is scoped to the triggering event's shop, so
    // the second counts only itself rather than continuing from the first.
    let scenario = format!(
        r#"
load("events/t.star", "placed")

cases = [
    case(
        name = "the fold is scoped to the triggering event",
        effect = "probe",
        given = [
            placed(id = "{A}", shop = 1),
            placed(id = "{B}", shop = 2),
        ],
        expect = [
            command_call("record", {{"id": "{A}", "shop": 1}}),
            command_call("record", {{"id": "{B}", "shop": 1}}),
        ],
    ),
]
"#
    );
    assert_eq!(run(effect, &scenario), ok());
}

/// The fold runs over `log[0..=N]`, so an effect that folds its own trigger type
/// counts itself. This is the semantics chosen (state is "the log at my position"),
/// so it is pinned rather than left to fall out of the implementation.
#[test]
fn the_fold_is_inclusive_of_the_triggering_event() {
    let effect = r#"
load("events/t.star", "placed")

def query(event):
    return [placed(shop = event.data.shop)]

initial = {"count": 0}

fold = {placed(): lambda state, event: {"count": state["count"] + 1}}

def probe(event, state):
    invoke_command("record", {"id": event.data.id, "shop": state["count"]})

handle = {placed(): probe}
"#;
    // Both events are in shop 1, so the second sees itself *and* the first.
    let scenario = format!(
        r#"
load("events/t.star", "placed")

cases = [
    case(
        name = "the first sees one, the second sees two",
        effect = "probe",
        given = [
            placed(id = "{A}", shop = 1),
            placed(id = "{B}", shop = 1),
        ],
        expect = [
            command_call("record", {{"id": "{A}", "shop": 1}}),
            command_call("record", {{"id": "{B}", "shop": 2}}),
        ],
    ),
]
"#
    );
    assert_eq!(run(effect, &scenario), ok());

    // The exclusive reading would have produced 0 then 1, so assert it does not.
    let exclusive = format!(
        r#"
load("events/t.star", "placed")

cases = [
    case(
        name = "not exclusive",
        effect = "probe",
        given = [placed(id = "{A}", shop = 1)],
        expect = [command_call("record", {{"id": "{A}", "shop": 0}})],
    ),
]
"#
    );
    assert_eq!(run(effect, &exclusive), failed());
}

/// An effect that needs no state declares no boundary, and its arm still takes the
/// parameter: one shape for every arm. With no `initial` either, that value is `None`.
#[test]
fn an_effect_without_a_boundary_sees_initial_or_none() {
    let with_initial = r#"
load("events/t.star", "placed")

initial = {"count": 7}

def probe(event, state):
    invoke_command("record", {"id": event.data.id, "shop": state["count"]})

handle = {placed(): probe}
"#;
    let scenario = format!(
        r#"
load("events/t.star", "placed")

cases = [
    case(
        name = "no query means initial",
        effect = "probe",
        given = [placed(id = "{A}", shop = 1)],
        expect = [command_call("record", {{"id": "{A}", "shop": 7}})],
    ),
]
"#
    );
    assert_eq!(run(with_initial, &scenario), ok());

    let bare = r#"
load("events/t.star", "placed")

def probe(event, state):
    if state != None:
        fail("state should be None with no initial, got " + str(state))
    invoke_command("record", {"id": event.data.id, "shop": 0})

handle = {placed(): probe}
"#;
    let bare_scenario = format!(
        r#"
load("events/t.star", "placed")

cases = [
    case(
        name = "no initial means None",
        effect = "probe",
        given = [placed(id = "{A}", shop = 1)],
        expect = [command_call("record", {{"id": "{A}", "shop": 0}})],
    ),
]
"#
    );
    assert_eq!(run(bare, &bare_scenario), ok());
}

/// Dispatch is fan-out, and the fold is of the log rather than of what an earlier arm
/// did, so every selecting arm sees the same state.
#[test]
fn two_arms_selecting_one_event_receive_the_same_state() {
    let effect = r#"
load("events/t.star", "placed")

def query(event):
    return [placed(shop = event.data.shop)]

initial = {"count": 0}

fold = {placed(): lambda state, event: {"count": state["count"] + 1}}

def first(event, state):
    invoke_command("record", {"id": event.data.id, "shop": state["count"]})

def second(event, state):
    invoke_command("record", {"id": event.data.id, "shop": state["count"] + 100})

handle = {
    placed(): first,
    placed(shop = 1): second,
}
"#;
    let scenario = format!(
        r#"
load("events/t.star", "placed")

cases = [
    case(
        name = "both arms fold the same boundary",
        effect = "probe",
        given = [placed(id = "{A}", shop = 1)],
        expect = [
            command_call("record", {{"id": "{A}", "shop": 1}}),
            command_call("record", {{"id": "{A}", "shop": 101}}),
        ],
    ),
]
"#
    );
    assert_eq!(run(effect, &scenario), ok());
}

/// A `handle` key is lowered without a keystore (one lowering serves a whole stream),
/// so it can only filter plaintext. A `query` and a `fold` key are lowered per
/// invocation with the real keystore, so a scoped subject constraint resolves there.
#[test]
fn a_subject_constraint_works_in_query_and_fold_but_not_in_a_handle_key() {
    let events = r#"
placed = event(
    type = "t.placed",
    fields = {
        "id": uuid(),
        "owner": uint(),
        "secret": str(subject = "owner", max_length = 50),
    },
)
noted = event(type = "t.noted", fields = {"id": uuid(), "shop": uint()})
"#;
    let files = |effect: &'static str| {
        vec![
            ("events/t.star", events),
            (
                "commands/record.star",
                r#"
load("events/t.star", "noted")

input = schema(id = uuid(), shop = uint())

def handle(input, state):
    return noted(id = input.id, shop = input.shop)
"#,
            ),
            ("effects/probe.star", effect),
        ]
    };

    // Scoped: the subject id is constrained too, so the runtime can find the key.
    let scoped = r#"
load("events/t.star", "placed")

def query(event):
    return [placed(owner = event.data.owner, secret = "s")]

initial = {"count": 0}

fold = {placed(owner = 1, secret = "s"): lambda state, event: {"count": state["count"] + 1}}

def probe(event, state):
    invoke_command("record", {"id": event.data.id, "shop": state["count"]})

handle = {placed(): probe}
"#;
    let project = write_project(&files(scoped));
    let findings = support::errors(&hekla::loader::LoadedProject::load(project.path()));
    assert!(
        findings.is_empty(),
        "a scoped subject filter is fine: {findings:?}"
    );

    // A `handle` key cannot filter an encrypted field at all.
    let in_handle = r#"
load("events/t.star", "placed")

def probe(event, state):
    invoke_command("record", {"id": event.data.id, "shop": 0})

handle = {placed(owner = 1, secret = "s"): probe}
"#;
    let project = write_project(&files(in_handle));
    let findings = support::errors(&hekla::loader::LoadedProject::load(project.path()));
    assert!(
        findings.iter().any(|err| err.contains("secret")),
        "a handle key must not filter an encrypted field: {findings:?}"
    );
}

/// The folded state an effect's `handle` receives is frozen, exactly as a command's is.
///
/// It became frozen when the fold started chunking (a chunk freezes its state and drops
/// its heap), and the two paths agreeing matters more than which way they agree: `state`
/// is derived from the log, so writing into it changes nothing durable and the write is
/// silently lost. Failing loudly is the honest outcome, and pinning it here keeps the
/// command and effect contracts from drifting apart again.
#[test]
fn an_effect_handle_cannot_mutate_the_folded_state() {
    // `run` yields only an exit code, so a bare "it failed" would also be satisfied by
    // a typo, a `query` error, or a renamed field. The control is the same effect with
    // the mutation removed and the expectation adjusted: it must pass. Only the one
    // line differs, so a failure of the first with a pass of the second isolates the
    // write to `state` as the cause.
    let effect = |mutate: bool| {
        format!(
            r#"
load("events/t.star", "placed")

def query(event):
    return [placed(shop = event.data.shop)]

initial = {{"count": 0}}

fold = {{placed(): lambda state, event: {{"count": state["count"] + 1}}}}

def probe(event, state):
{}
    invoke_command("record", {{"id": event.data.id, "shop": state["count"]}})

handle = {{placed(): probe}}
"#,
            if mutate {
                "    state[\"count\"] = 99"
            } else {
                "    pass"
            }
        )
    };
    let scenario = |shop: u32| {
        format!(
            r#"
load("events/t.star", "placed")

cases = [
    case(
        name = "the arm reads the folded state",
        effect = "probe",
        given = [placed(id = "{A}", shop = 1)],
        expect = [command_call("record", {{"id": "{A}", "shop": {shop}}})],
    ),
]
"#
        )
    };
    assert_eq!(
        run(&effect(false), &scenario(1)),
        ok(),
        "the control must pass, or the mutating case proves nothing"
    );
    assert_eq!(run(&effect(true), &scenario(99)), failed());
}
