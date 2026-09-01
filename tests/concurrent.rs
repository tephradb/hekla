//! What holds regardless of scheduling.
//!
//! Parallel submission breaks the determinism `tests/model.rs` depends on, so it does
//! not belong inside a generated sequence: a shrinker cannot shrink a race, and a model
//! that has to predict an interleaving is predicting the scheduler. These are the
//! properties that are true of the *set* of outcomes rather than of any one of them,
//! and they are the ones worth asserting under contention.
//!
//! Neither case asserts that a particular interleaving happened. A test that needs one
//! is a test that fails on a quiet machine, so what is asserted is the invariant and
//! what is printed is the split.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use hekla::effect::StubHttpClient;
use serde_json::{Value, json};

mod support;

use support::{Boot, Harness, ctx, fixture_dir, quiesce, read_row};

/// The fixture's org allocation, from `lib/config.hk`.
const ORG_CAP: usize = 6;

/// Twice the cap, so the boundary is genuinely contended and the losers have somewhere
/// to lose to.
const WRITERS: usize = 12;

fn boot() -> Harness {
    Boot::new(fixture_dir("tickets"))
        .with_master_key()
        .http_status(200)
        .start()
}

fn open_body(ticket: &str, org: i64, owner: i64, contact: Option<&str>) -> Value {
    json!({
        "ticket_id": ticket,
        "org_id": org,
        "owner_id": owner,
        "title": "the printer is on fire",
        "priority": "Urgent",
        "due_at": 1_700_000_000_000_000i64,
        "fee": "12.50",
        "budget": "900.00",
        "contact": contact,
        "meta": {},
    })
}

fn field<'a>(row: &'a Value, name: &str) -> &'a Value {
    row.get(name)
        .unwrap_or_else(|| panic!("row {row} has no field `{name}`"))
}

/// The cap is a rule about every ticket in an organisation, so it has to hold at append
/// time. Twelve threads race for six places and the arithmetic has to come out exactly:
/// the cap is never exceeded, no ticket is opened twice, and the log holds one event per
/// success and nothing else.
///
/// No contacts, so the effect logs and returns without invoking. That keeps `log_head`
/// equal to the number of successes, which is the assertion: a duplicate append would
/// show up there even if the read model happened to collapse it.
#[test]
fn twelve_writers_racing_for_six_places_never_exceed_the_cap() {
    let harness = boot();
    let rt = Arc::clone(&harness.rt);

    let statuses: Vec<u16> = thread::scope(|scope| {
        let handles: Vec<_> = (0..WRITERS)
            .map(|n| {
                let rt = Arc::clone(&rt);
                scope.spawn(move || {
                    let ticket = uuid::Uuid::from_u128(n as u128 + 1).to_string();
                    let result = rt
                        .execute("OpenTicket", open_body(&ticket, 1, 10, None), &ctx(), None)
                        .expect("the command should run");
                    (result.status, result.body)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                let (status, body) = handle.join().expect("no writer should panic");
                assert!(
                    matches!(status, 200 | 422 | 409),
                    "a racing writer answered {status}: {body}"
                );
                if status == 422 {
                    assert_eq!(
                        body["error"]["code"], "org_full",
                        "the only refusal this command has is the cap: {body}"
                    );
                }
                status
            })
            .collect()
    });

    let opened = statuses.iter().filter(|status| **status == 200).count();
    let refused = statuses.iter().filter(|status| **status == 422).count();
    let conflicted = statuses.iter().filter(|status| **status == 409).count();
    println!("{opened} opened, {refused} refused, {conflicted} conflicted");

    assert!(
        opened <= ORG_CAP,
        "{opened} tickets were opened against a cap of {ORG_CAP}"
    );
    assert_eq!(
        opened + refused + conflicted,
        WRITERS,
        "every writer must come back with one of the three answers"
    );

    quiesce(&harness);
    assert_eq!(
        harness.rt.log_head() as usize,
        opened,
        "one event per success and nothing else: a duplicate append would land here"
    );

    let totals = read_row(&harness, "Tickets", "OrgTotals", "1", harness.rt.log_head())
        .expect("the organisation should have a row");
    assert_eq!(
        field(&totals, "opened").as_u64().unwrap() as usize,
        opened,
        "the read model counts what the log holds"
    );

    // And each success really is a distinct ticket, which the count above cannot say on
    // its own: six appends of one id would satisfy it.
    let rows = (0..WRITERS)
        .filter(|n| {
            let ticket = uuid::Uuid::from_u128(*n as u128 + 1).to_string();
            read_row(&harness, "Tickets", "Ticket", &ticket, 0).is_some()
        })
        .count();
    assert_eq!(rows, opened, "each success is its own ticket");

    harness.shutdown();
}

/// An erasure racing an effect that is about to read the erased address.
///
/// Two outcomes are legitimate and the runtime picks whichever wins: the notification
/// goes out with the plaintext, or the invocation is terminally skipped because rule 12
/// makes an erased `reveal` unrecoverable. What must never happen is a third thing, and
/// a third thing is exactly what a subtle bug here would look like: a call carrying
/// ciphertext, a call carrying an empty address, or a wedged lane that retries forever.
///
/// This is the case `tests/model.rs` quiesces before every erase to keep out of the
/// deterministic comparison, because "delivered or skipped" is a property of the set of
/// outcomes and not something a model can predict.
#[test]
fn an_erasure_racing_an_effect_either_delivers_the_plaintext_or_skips() {
    const TRIALS: usize = 16;
    const ADDRESS: &str = "ada@example.com";

    let delivered = AtomicUsize::new(0);
    let skipped = AtomicUsize::new(0);

    for trial in 0..TRIALS {
        let stub = Arc::new(StubHttpClient::ok());
        let harness = Boot::new(fixture_dir("tickets"))
            .with_master_key()
            .http(Arc::clone(&stub) as Arc<dyn hekla::http::HttpClient>)
            .start();

        let ticket = uuid::Uuid::from_u128(trial as u128 + 1).to_string();
        let owner = 100 + trial as i64;
        let result = harness
            .rt
            .execute(
                "OpenTicket",
                open_body(&ticket, 1, owner, Some(ADDRESS)),
                &ctx(),
                None,
            )
            .unwrap();
        assert_eq!(result.status, 200, "{:?}", result.body);

        // A graduated pause rather than none at all. With no pause the erase wins every
        // time on this machine, which leaves the delivered half of the assertion never
        // executed; a spread that straddles the lane's own latency exercises both.
        thread::sleep(Duration::from_micros(trial as u64 * 500));
        harness
            .rt
            .keystore()
            .unwrap()
            .erase("owner_id", &owner.to_string())
            .unwrap();

        quiesce(&harness);

        let calls = stub.calls();
        let terminal = harness.rt.effect("NotifyOwner").unwrap().terminal_skips();
        match calls.len() {
            0 => {
                assert_eq!(
                    terminal, 1,
                    "an undelivered notification has to be a terminal skip, not a silent drop"
                );
                skipped.fetch_add(1, Ordering::Relaxed);
            }
            1 => {
                let sent: Value = serde_json::from_slice(calls[0].body.as_ref().unwrap()).unwrap();
                assert_eq!(
                    field(&sent, "to"),
                    &json!(ADDRESS),
                    "a notification that went out went out with the plaintext, never with \
                     ciphertext or a blank"
                );
                assert_eq!(terminal, 0, "a delivered invocation is not also a skip");
                delivered.fetch_add(1, Ordering::Relaxed);
            }
            other => panic!("the effect sent {other} requests for one ticket"),
        }

        harness.shutdown();
    }

    // Printed rather than asserted. Which side wins is the scheduler's business, and a
    // test that required both would fail on a machine quiet enough to always deliver.
    println!(
        "{} delivered, {} skipped over {TRIALS} trials",
        delivered.load(Ordering::Relaxed),
        skipped.load(Ordering::Relaxed)
    );
}
