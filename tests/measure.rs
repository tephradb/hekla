//! Two numbers the port owes, not assertions. Ignored by default; run with
//! `cargo test --release --test measure -- --ignored --nocapture`.
//!
//! What they answered, on 20,000 events:
//!
//! - **A full projector rebuild: 228ms**, over two entities (one `put` and one `patch`
//!   per event, so 40,000 writes of which 20,000 read the row first). About 11µs an
//!   event. `patch` producing a whole row, and every write therefore being a SELECT
//!   then an INSERT OR REPLACE, is not what a rebuild is spending its time on, so
//!   `apply_one`'s UPDATE arm stays dormant.
//! - **A command fold over an encrypted boundary: 25ms, against 25ms for the same fold
//!   over plaintext.** There is no difference left to measure, which is what this number
//!   is now for rather than a failure of it.
//!
//!   It was **93ms against 24ms**. The adapter decrypted every subject-scoped field of
//!   every record a fold read, 3.5µs a record, which made a fold four times its own cost
//!   for content the fold never looked at. Starlark never did that (a handle kept
//!   ciphertext opaque) and the heklang port regressed it, because `Value::Sealed` held
//!   plaintext and something had to put it there. heklang now carries the stored form
//!   and `Keys::decrypt` opens it at the one `reveal` that asks, so a fold pays for what
//!   it reads and this fold reads none of it.

use std::time::Instant;

use serde_json::json;

mod support;

use support::{Boot, ctx, wait_position, write_project};

const N: usize = 20_000;

/// The whole-row `patch` decision: every write is an INSERT OR REPLACE preceded by a
/// SELECT, and the question was what a rebuild of that costs.
#[test]
#[ignore]
fn a_full_projector_rebuild() {
    let dir = write_project(&[
        (
            "events/thing.hk",
            "event @thing.happened { id: Uuid, n: Int }\n",
        ),
        (
            "commands/do-thing.hk",
            "command DoThing(id: Uuid, n: Int) {\n  emit @thing.happened { id, n }\n}\n",
        ),
        (
            "projectors/things.hk",
            r#"
projector Things {
  entity Thing {
    id: Uuid @key,
    n: Int,
  }

  on @thing.happened { id, n } {
    put Thing { id, n }
  }

  entity Total {
    key: String @key,
    seen: Int,
  }

  on @thing.happened {
    patch Total["all"] { seen: .seen + 1 }
  }
}
"#,
        ),
    ]);
    let harness = Boot::new(dir.path()).start();
    let started = Instant::now();
    for i in 0..N {
        let id = uuid::Uuid::from_u128(i as u128 + 1).to_string();
        let result = harness
            .rt
            .execute("DoThing", json!({ "id": id, "n": i }), &ctx(), None)
            .unwrap();
        assert_eq!(result.status, 200, "{:?}", result.body);
    }
    let appended = started.elapsed();
    wait_position(&harness.rt, "Things", N as u64);
    println!("appended {N} events in {appended:?}");

    // The live position never drops during a rebuild (it happens into a sibling file
    // and is swapped in by rename), so the sibling appearing and vanishing is what
    // brackets it.
    let sibling = harness
        .rt
        .projector("Things")
        .unwrap()
        .db_path
        .with_extension("rebuild.db");
    let started = Instant::now();
    harness.rt.projector("Things").unwrap().request_replay();
    while !sibling.exists() {
        std::hint::spin_loop();
    }
    let began = started.elapsed();
    while sibling.exists() {
        std::hint::spin_loop();
    }
    println!(
        "rebuilt {N} events in {:?} (picked up after {began:?})",
        started.elapsed() - began
    );
    harness.shutdown();
}

/// What a boundary of encrypted records costs a fold that reads none of them. The
/// answer is nothing, and the two halves below are here to keep it that way: a
/// regression to eager decryption shows up as the second number pulling away from the
/// first, which is exactly how this was found.
#[test]
#[ignore]
fn a_command_fold_over_an_encrypted_boundary() {
    let dir = write_project(&[
        (
            "events/thing.hk",
            r#"
event @thing.happened {
  id: Uuid,
  shop: Int,
  secret: String? @subject(shop) @max(100),
}
"#,
        ),
        (
            "commands/do-thing.hk",
            r#"
command DoThing(id: Uuid, shop: Int, secret: String?) {
  emit @thing.happened { id, shop, secret }
}
"#,
        ),
        (
            "commands/count-thing.hk",
            r#"
command CountThing(id: Uuid, shop: Int) {
  state seen: Int = fold 0
    on @thing.happened(shop) => seen + 1

  if seen >= 0 {
    return reject("counted", "{seen}")
  }

  emit @thing.happened { id, shop, secret: none }
}
"#,
        ),
    ]);
    let harness = Boot::new(dir.path()).with_master_key().start();
    for i in 0..N {
        let id = uuid::Uuid::from_u128(i as u128 + 1).to_string();
        let result = harness
            .rt
            .execute(
                "DoThing",
                json!({ "id": id, "shop": 1, "secret": "a-personal-value" }),
                &ctx(),
                None,
            )
            .unwrap();
        assert_eq!(result.status, 200, "{:?}", result.body);
    }

    let started = Instant::now();
    let result = harness
        .rt
        .execute(
            "CountThing",
            json!({ "id": uuid::Uuid::nil().to_string(), "shop": 1 }),
            &ctx(),
            None,
        )
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(result.body["error"]["message"], N.to_string());
    println!("folded {N} encrypted records in {elapsed:?}");
    harness.shutdown();

    // The same fold with nothing to decrypt, so the difference is the decrypt.
    let dir = write_project(&[
        (
            "events/thing.hk",
            "event @thing.happened { id: Uuid, shop: Int, secret: String? @max(100) }\n",
        ),
        (
            "commands/do-thing.hk",
            r#"
command DoThing(id: Uuid, shop: Int, secret: String?) {
  emit @thing.happened { id, shop, secret }
}
"#,
        ),
        (
            "commands/count-thing.hk",
            r#"
command CountThing(id: Uuid, shop: Int) {
  state seen: Int = fold 0
    on @thing.happened(shop) => seen + 1

  if seen >= 0 {
    return reject("counted", "{seen}")
  }

  emit @thing.happened { id, shop, secret: none }
}
"#,
        ),
    ]);
    let harness = Boot::new(dir.path()).start();
    for i in 0..N {
        let id = uuid::Uuid::from_u128(i as u128 + 1).to_string();
        harness
            .rt
            .execute(
                "DoThing",
                json!({ "id": id, "shop": 1, "secret": "a-personal-value" }),
                &ctx(),
                None,
            )
            .unwrap();
    }
    let started = Instant::now();
    harness
        .rt
        .execute(
            "CountThing",
            json!({ "id": uuid::Uuid::nil().to_string(), "shop": 1 }),
            &ctx(),
            None,
        )
        .unwrap();
    println!("folded {N} plaintext records in {:?}", started.elapsed());
    harness.shutdown();
}

/// The retry budget against contention on a wide boundary.
///
/// A wide boundary conflicts by design: every writer folds the whole shop, so every
/// append moves the boundary every other writer is holding. The retry loop absorbs
/// that up to `HEKLA_MAX_ATTEMPTS` and then answers 409, which is correct but is the
/// number an operator needs before they put a hot boundary behind it.
///
/// Measured with 15 requests a thread, cap far above the load so nothing rejects:
///
/// | writers | attempts=5 (default) | attempts=10 | attempts=20 |
/// | ---: | ---: | ---: | ---: |
/// | 2 | 0.0% | | |
/// | 4 | 3.3% | | |
/// | 8 | 15.0% | 0.8% | 0.0% |
/// | 16 | 31.2% | 5.4% | 1.7% |
/// | 32 | 53.3% | 15.2% | 6.7% |
///
/// Correctness never moved: at every point the log held exactly the successes and no
/// attempt returned anything but 200 or 409. The default budget of 5 is sized for
/// about four concurrent writers per boundary at a conflict rate under 5%.
#[test]
#[ignore = "a measurement, not an assertion"]
fn contention_against_the_retry_budget() {
    const PER_THREAD: usize = 15;
    let project = write_project(&[
        (
            "events/order.hk",
            "event @order.placed { order_id: Uuid, shop_id: Int }\n",
        ),
        (
            "commands/place-order.hk",
            r#"
command PlaceOrder(order_id: Uuid, shop_id: Int) {
  state placed: Bool = fold false
    on @order.placed(order_id) => true
  state sold: Int = fold 0
    on @order.placed(shop_id) => sold + 1

  if placed {
    return
  }
  if sold >= 100000 {
    return reject("sold_out", "gone")
  }
  emit @order.placed { order_id, shop_id }
}
"#,
        ),
    ]);

    for threads in [2usize, 4, 8, 16, 32] {
        let data = tempfile::tempdir().unwrap();
        let harness = Boot::new(project.path()).data_dir(data.path()).start();
        let started = Instant::now();
        let counts: Vec<(usize, usize, usize)> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..threads)
                .map(|t| {
                    let rt = &harness.rt;
                    scope.spawn(move || {
                        let (mut ok, mut conflict, mut other) = (0usize, 0usize, 0usize);
                        for i in 0..PER_THREAD {
                            let id = format!("{:08x}-1111-4111-8111-111111111111", t * 1000 + i);
                            match rt.execute(
                                "PlaceOrder",
                                json!({ "order_id": id, "shop_id": 1 }),
                                &ctx(),
                                None,
                            ) {
                                Ok(result) if result.status == 200 => ok += 1,
                                Ok(result) if result.status == 409 => conflict += 1,
                                _ => other += 1,
                            }
                        }
                        (ok, conflict, other)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let elapsed = started.elapsed();
        let ok: usize = counts.iter().map(|count| count.0).sum();
        let conflict: usize = counts.iter().map(|count| count.1).sum();
        let other: usize = counts.iter().map(|count| count.2).sum();
        let total = threads * PER_THREAD;
        println!(
            "{threads:>2} writers: ok={ok:>3} conflict={conflict:>3} \
             ({:>4.1}%) other={other} in {elapsed:?}",
            100.0 * conflict as f64 / total as f64
        );
        assert_eq!(support::log_head(&harness.rt) as usize, ok);
        assert_eq!(other, 0, "only 200 and 409 are reachable here");
        harness.shutdown();
    }
}

/// What a conflict costs when the boundary is deep, which is the case the incremental
/// carry exists for and the one `contention_against_the_retry_budget` cannot show: its
/// boundary never gets past a few hundred events, so re-folding it is nearly free and the
/// backoff sleep is most of what either column measures.
///
/// The log is seeded past `DEPTH` first, through a command that declares no `state` so
/// seeding stays linear, and the contended command folds that whole slice on every
/// attempt. Run it against a build with the carry ablated (reset `carried` and
/// `folded_through` at the end of each attempt in heklang's `execute`) for the other
/// column.
///
/// On 20,000 seeded events, 15 commands per writer, same machine, one run each. The
/// committed counts matter more than the times: re-folding from zero spends the retry
/// budget on the boundary and answers 409 to callers a cheap retry would have settled.
///
/// | writers | with the carry | re-folding from zero |
/// | --- | --- | --- |
/// | 4 | 315ms, 60/60 committed | 940ms, 46/60 |
/// | 16 | 530ms, 238/240 committed | 2.07s, 93/240 |
/// | 32 | 1.02s, 427/480 committed | 4.36s, 106/480 |
#[test]
#[ignore = "a measurement, not an assertion"]
fn contention_against_a_deep_boundary() {
    const DEPTH: usize = 20_000;
    const PER_THREAD: usize = 15;
    let project = write_project(&[
        (
            "events/order.hk",
            "event @order.placed { order_id: Uuid, shop_id: Int }\n",
        ),
        // No `state`, so seeding the boundary does not fold it: 20,000 appends rather
        // than 200 million record reads.
        (
            "commands/seed-order.hk",
            r#"
command SeedOrder(order_id: Uuid, shop_id: Int) {
  emit @order.placed { order_id, shop_id }
}
"#,
        ),
        (
            "commands/place-order.hk",
            r#"
command PlaceOrder(order_id: Uuid, shop_id: Int) {
  state sold: Int = fold 0
    on @order.placed(shop_id) => sold + 1

  emit @order.placed { order_id, shop_id }
}
"#,
        ),
    ]);
    let harness = Boot::new(project.path()).start();
    for i in 0..DEPTH {
        let id = uuid::Uuid::from_u128(i as u128 + 1).to_string();
        let result = harness
            .rt
            .execute(
                "SeedOrder",
                json!({ "order_id": id, "shop_id": 1 }),
                &ctx(),
                None,
            )
            .unwrap();
        assert_eq!(result.status, 200, "{:?}", result.body);
    }

    // One harness for every round: the rounds add a few hundred events to a boundary of
    // twenty thousand, which is not enough to make the later ones a different measurement.
    let mut next = DEPTH;
    for threads in [4usize, 16, 32] {
        let base = next;
        next += threads * PER_THREAD;
        let started = Instant::now();
        let counts: Vec<(usize, usize)> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..threads)
                .map(|t| {
                    let rt = &harness.rt;
                    scope.spawn(move || {
                        let (mut ok, mut conflict) = (0usize, 0usize);
                        for i in 0..PER_THREAD {
                            let id = uuid::Uuid::from_u128((base + t * PER_THREAD + i) as u128 + 1);
                            let result = rt
                                .execute(
                                    "PlaceOrder",
                                    json!({ "order_id": id.to_string(), "shop_id": 1 }),
                                    &ctx(),
                                    None,
                                )
                                .unwrap();
                            match result.status {
                                200 => ok += 1,
                                409 => conflict += 1,
                                other => panic!("unexpected {other}: {:?}", result.body),
                            }
                        }
                        (ok, conflict)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let elapsed = started.elapsed();
        let ok: usize = counts.iter().map(|count| count.0).sum();
        let conflict: usize = counts.iter().map(|count| count.1).sum();
        let total = threads * PER_THREAD;
        println!(
            "{threads:>2} writers over {DEPTH} events: ok={ok:>3} conflict={conflict:>3} \
             ({:>4.1}%) in {elapsed:?}",
            100.0 * conflict as f64 / total as f64
        );
    }
    harness.shutdown();
}
