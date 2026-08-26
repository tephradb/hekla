//! The chunked fold: a boundary deeper than one chunk's heap budget.
//!
//! Its own test binary, deliberately. The anti-vacuity guards below read the process
//! counters in [`hekla::dispatch::fold_counters`], and integration tests in one file
//! share a process while running on parallel threads, so a sibling test folding
//! anything would satisfy them on this test's behalf. One test in one binary is what
//! makes "this test crossed a seam" mean what it says.

use serde_json::json;
use uuid::Uuid;

mod support;

use support::{ALICE, Boot, ctx, log_head, write_project};

/// Run a command that must fail at dispatch and return its rendered error.
/// `ExecResult` is not `Debug`, so `Result::unwrap_err` is unavailable here.
fn exec_err(rt: &hekla::runtime::Runtime, command: &str, body: serde_json::Value) -> String {
    match rt.execute(command, body, &ctx(), None) {
        Ok(result) => panic!(
            "`{command}` should have failed at dispatch, got status {} {:?}",
            result.status, result.body
        ),
        Err(err) => format!("{err:#}"),
    }
}

/// A padded event. The pad is what makes a boundary of a few hundred of these
/// outgrow one fold chunk's heap budget, which is the point: the chunking is
/// invisible in the result, so the only way to exercise it is to allocate past it.
const PADDED_EVENTS: &str = r#"
noted = event(
    type = "t.noted",
    fields = {
        "id": uuid(),
        "owner": str(max_length = 50),
        "pad": str(max_length = 8000, indexed = False),
    },
)
"#;

/// Seeds the boundary in batches, and reports what it folded when asked. The state
/// carries the first and last id it saw, so a carry lost at a chunk boundary shows up
/// as a wrong id rather than only as a wrong count.
const PADDED_FOLD: &str = r#"
load("events/t.star", "noted")

PAD = "x" * 4000
BATCH = 50

input = schema(id = uuid(), owner = str(), batch = uint())

def query(input):
    return noted(owner = input.owner)

initial = {"seen": 0, "first": None, "last": None}

fold = {
    noted(): lambda state, event: dict(
        state,
        seen = state["seen"] + 1,
        first = state["first"] if state["first"] != None else event.data.id,
        last = event.data.id,
    ),
}

def handle(input, state):
    if input.batch == 999:
        return reject("report", "seen %d first %s last %s" % (
            state["seen"],
            state["first"],
            state["last"],
        ))
    return [
        noted(
            id = uuid5(input.id, "%d-%d" % (input.batch, i)),
            owner = input.owner,
            pad = PAD,
        )
        for i in range(BATCH)
    ]
"#;

/// Starlark collects only at a module's top level, so a fold releases nothing until
/// its heap is dropped: one heap for the whole boundary costs memory linear in depth.
/// The fold therefore freezes and drops its heap every `HEKLA_FOLD_HEAP_BUDGET` bytes,
/// which is only sound if the state carries across that seam intact. 600 padded events
/// is several chunks at the default budget.
/// A fold arm that mutates the state a *previous* arm call built, rather than the
/// frozen `initial`. Legal-looking, forbidden by the contract, and it used to work.
const MUTATING_ACCUMULATOR: &str = r#"
load("events/t.star", "noted")

input = schema(id = uuid(), owner = str(), batch = uint())

def query(input):
    return noted(owner = input.owner)

initial = None

def accumulate(state, event):
    if state == None:
        return {"seen": 1}
    state["seen"] = state["seen"] + 1
    return state

fold = {noted(): accumulate}

def handle(input, state):
    return reject("report", "seen %d" % state["seen"])
"#;

/// Both halves of the chunk seam, in one test because they share a process and the
/// counters that prove they are not vacuous.
///
/// **The carry is intact across a seam.** Starlark collects only at a module's top
/// level, so a fold releases nothing until its heap is dropped: one heap for the whole
/// boundary costs memory linear in depth. The fold therefore freezes and drops its heap
/// every `HEKLA_FOLD_HEAP_BUDGET` bytes, which is only sound if the state survives that
/// seam whole. Folding in chunks and folding in one pass give the same state by
/// construction, so this asserts the counters moved too: otherwise it passes with the
/// budget raised past anything the project allocates, and proves nothing.
///
/// **A fold that mutates its own accumulated state fails once it crosses one.**
/// `AUTHORING.md` has always said to return the new state, and Phase 15 already made a
/// mutating arm fail on any retry. Chunking extends that to a deep first attempt, so
/// the same fold succeeds shallow and fails deep. That is worth being deliberate about:
/// the alternative, freezing after every event, would make it uniform at the cost of a
/// freeze per event, and the pattern is contract-breaking either way.
#[test]
fn the_chunk_seam_carries_state_and_refuses_mutation() {
    const BATCHES: u32 = 12;
    const PER_BATCH: u32 = 50;

    let project = write_project(&[
        ("events/t.star", PADDED_EVENTS),
        ("commands/note.star", PADDED_FOLD),
        ("commands/tally.star", MUTATING_ACCUMULATOR),
    ]);
    let harness = Boot::new(project.path()).start();
    let seed = |batch: u32| {
        let out = harness
            .rt
            .execute(
                "note",
                json!({ "id": ALICE, "owner": "kim", "batch": batch }),
                &ctx(),
                None,
            )
            .unwrap();
        assert_eq!(out.status, 200, "batch {batch}: {:?}", out.body);
    };
    let tally = || {
        harness.rt.execute(
            "tally",
            json!({ "id": ALICE, "owner": "kim", "batch": 0 }),
            &ctx(),
            None,
        )
    };

    // One batch is a single chunk, so a mutating arm never meets a seam and the
    // contract-breaking fold still works. This is the control for the deep case.
    seed(0);
    let (_, seams_shallow) = hekla::dispatch::fold_counters();
    let shallow = tally().unwrap();
    assert_eq!(shallow.status, 422, "{:?}", shallow.body);
    assert_eq!(shallow.body["error"]["message"], "seen 50");
    let (_, seams_after_shallow) = hekla::dispatch::fold_counters();
    assert_eq!(
        seams_after_shallow, seams_shallow,
        "the shallow case is only a control if it stays inside one chunk"
    );

    let (events_before, seams_before) = hekla::dispatch::fold_counters();
    for batch in 1..BATCHES {
        seed(batch);
    }

    // The carry: the count, and the first and last id the fold saw, so a state dropped
    // at a seam shows up as a wrong id rather than only as a wrong total.
    let report = harness
        .rt
        .execute(
            "note",
            json!({ "id": ALICE, "owner": "kim", "batch": 999 }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(report.status, 422, "{:?}", report.body);
    let namespace = Uuid::parse_str(ALICE).unwrap();
    let derive = |name: &str| Uuid::new_v5(&namespace, name.as_bytes());
    assert_eq!(
        report.body["error"]["message"],
        format!(
            "seen {} first {} last {}",
            BATCHES * PER_BATCH,
            derive("0-0"),
            derive(&format!("{}-{}", BATCHES - 1, PER_BATCH - 1)),
        )
    );
    assert_eq!(log_head(&harness.rt), (BATCHES * PER_BATCH) as u64);

    // The same fold that worked at 50 events now fails at 600.
    let err = exec_err(
        &harness.rt,
        "tally",
        json!({ "id": ALICE, "owner": "kim", "batch": 0 }),
    );
    assert!(
        err.contains("fold returns the new state"),
        "a bare `Immutable` is not a usable message, got: {err}"
    );

    let (events_after, seams_after) = hekla::dispatch::fold_counters();
    assert!(
        events_after > events_before,
        "the fold counter did not move at all"
    );
    assert!(
        seams_after > seams_before,
        "no chunk seam was crossed, so neither half of this test proves anything"
    );
    harness.shutdown();
}
