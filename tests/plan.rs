//! `hekla plan`: what a deploy would change, before it changes it.
//!
//! Every test here is two deploys. The first boots a project so the data directory
//! records it; the second rewrites the source and plans against the same directory
//! without booting. That is the shape of the question the command answers, and it is
//! the only shape that can catch a plan which is right about a fresh directory and
//! wrong about a deployed one.
//!
//! Following `verify`'s rule: a checker that never fires is indistinguishable from one
//! that works, so the edits that must move nothing are tested beside the ones that must.

mod support;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use hekla::crypto::MasterKeys;
use hekla::effect::{Replayed, StubHttpClient};
use hekla::http::HttpResponse;
use hekla::plan::{self, Cause, Rebuild, Verdict};
use heklang::Kind;
use serde_json::{Value, json};
use support::{
    Boot, Harness, ctx, load_ok, quiesce, wait_effect_position, wait_until, write_project,
};

// --- fixtures --------------------------------------------------------------

const EVENTS: &str = "\
event @shop.connected { shop_id: Int }
event @thing.done { shop_id: Int }
";

/// A refusal and the guard that rejects with it. Neither has a digest entry of its
/// own, so both are the fan-out this command has to explain.
const RULES: &str = "\
refusal ShopNotConnected \"the shop is not connected\"

guard ShopIsConnected(shop_id: Int) {
  fold connected: Bool = false
    on @shop.connected(shop_id) => true

  if !connected {
    return reject ShopNotConnected
  }
}
";

const DO_A: &str = "\
command DoA(shop_id: Int) {
  guard ShopIsConnected { shop_id }
  emit @thing.done { shop_id }
}
";

const DO_B: &str = "\
command DoB(shop_id: Int) {
  guard ShopIsConnected { shop_id }
  emit @thing.done { shop_id }
}
";

/// Names no guard, so it is the control: an edit to the guard must leave it alone.
const DO_C: &str = "\
command DoC(shop_id: Int) {
  fold seen: Int = 0
    on @thing.done(shop_id) => seen + 1

  emit @thing.done { shop_id }
}
";

const PROJECTOR: &str = "\
projector Things {
  entity Thing {
    shop_id: Int @key,
  }

  on @thing.done { shop_id } {
    put Thing { shop_id }
  }
}
";

fn files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("events/e.hk", EVENTS),
        ("lib/rules.hk", RULES),
        ("commands/a.hk", DO_A),
        ("commands/b.hk", DO_B),
        ("commands/c.hk", DO_C),
        ("projectors/things.hk", PROJECTOR),
    ]
}

/// Write the standard fixture into `dir`. An override replaces the file at that path,
/// or adds one the fixture does not have.
fn write(dir: &Path, overrides: &[(&str, &str)]) {
    let mut written = Vec::new();
    for (rel, content) in files() {
        let content = overrides
            .iter()
            .find(|(path, _)| *path == rel)
            .map(|(_, replacement)| *replacement)
            .unwrap_or(content);
        write_one(dir, rel, content);
        written.push(rel);
    }
    for (rel, content) in overrides {
        if !written.contains(rel) {
            write_one(dir, rel, content);
        }
    }
}

fn write_one(dir: &Path, rel: &str, content: &str) {
    let path = dir.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Boot once so the data directory records the project, then shut down.
fn deploy(project: &Path, data: &Path) {
    Boot::new(project).data_dir(data).start().shutdown();
}

fn boot(project: &Path, data: &Path) -> Harness {
    Boot::new(project).data_dir(data).start()
}

/// Plan `project` against `data` without booting anything.
fn plan_of(project: &Path, data: &Path) -> plan::Plan {
    plan::compute_with(&load_ok(project), data, plan::Replay::Off).expect("planning should succeed")
}

fn change<'a>(plan: &'a plan::Plan, name: &str) -> Option<&'a plan::Change> {
    plan.changes.iter().find(|change| change.name == name)
}

fn forecast<'a>(plan: &'a plan::Plan, name: &str) -> &'a plan::Forecast {
    plan.projectors
        .iter()
        .find(|forecast| forecast.name == name)
        .unwrap_or_else(|| panic!("no forecast for `{name}`"))
}

// --- the edits that must move nothing ---------------------------------------

#[test]
fn an_identical_redeploy_plans_nothing() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write(project.path(), &[]);
    deploy(project.path(), data.path());

    let plan = plan_of(project.path(), data.path());
    assert!(
        plan.is_empty(),
        "expected no changes, got {:?}",
        plan.changes
    );
    // An empty plan over nothing compared is a directory that was never deployed to,
    // which must not read the same as a project that genuinely matches.
    assert!(
        plan.declarations_compared >= 6,
        "expected the whole program compared, got {}",
        plan.declarations_compared
    );
}

/// The property Phase 23 bought, now visible at deploy time: the digest hashes what a
/// declaration does, so layout is not a change. Every scheme it replaced fails this.
#[test]
fn a_reformat_plans_nothing() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write(project.path(), &[]);
    deploy(project.path(), data.path());

    // Reindent, comment, and rename a local binding in the command that has one.
    let reformatted = "\
// a comment that changes nothing about what this does
command DoC(shop_id: Int) {
      fold tally: Int = 0
            on @thing.done(shop_id) => tally + 1

      emit @thing.done { shop_id }
}
";
    write(project.path(), &[("commands/c.hk", reformatted)]);

    let plan = plan_of(project.path(), data.path());
    assert!(
        plan.is_empty(),
        "a reformat should move nothing, got {:?}",
        plan.changes
    );
    assert_eq!(forecast(&plan, "Things").outcome, Rebuild::Resume);
}

// --- behaviour against contract ---------------------------------------------

#[test]
fn a_body_edit_is_behaviour_and_a_new_parameter_is_contract() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write(project.path(), &[]);
    deploy(project.path(), data.path());

    // The fold counts differently. Its parameters and its refusals did not move, so
    // nothing outside the program can tell.
    let body = DO_C.replace("seen + 1", "seen + 2");
    write(project.path(), &[("commands/c.hk", &body)]);
    let plan = plan_of(project.path(), data.path());
    assert_eq!(
        change(&plan, "DoC").map(|change| change.verdict),
        Some(Verdict::Behaviour),
        "a body-only edit should not move the contract: {plan}"
    );

    // A new parameter is visible to every caller.
    let signature = DO_C.replace("DoC(shop_id: Int)", "DoC(shop_id: Int, note: String)");
    write(project.path(), &[("commands/c.hk", &signature)]);
    let plan = plan_of(project.path(), data.path());
    assert_eq!(
        change(&plan, "DoC").map(|change| change.verdict),
        Some(Verdict::Contract),
        "a new parameter should move the contract: {plan}"
    );
}

#[test]
fn an_added_and_a_removed_declaration_are_both_reported() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write(project.path(), &[]);
    deploy(project.path(), data.path());

    let events = format!("{EVENTS}event @thing.undone {{ shop_id: Int }}\n");
    write(project.path(), &[("events/e.hk", &events)]);
    fs::remove_file(project.path().join("commands/b.hk")).unwrap();

    let plan = plan_of(project.path(), data.path());
    assert_eq!(
        change(&plan, "@thing.undone").map(|change| change.verdict),
        Some(Verdict::Added),
        "{plan}"
    );
    let removed = change(&plan, "DoB").expect("DoB should be reported as removed");
    assert_eq!(removed.verdict, Verdict::Removed);
    assert_eq!(removed.kind, Kind::Command);
    // A removal has a deployed form and no candidate one, which is what lets a reader
    // see what is about to stop existing.
    assert!(removed.before.is_some() && removed.after.is_none());
}

// --- the rebuild forecast ----------------------------------------------------

#[test]
fn a_handler_edit_forecasts_a_rebuild_and_a_cosmetic_one_does_not() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write(project.path(), &[]);
    deploy(project.path(), data.path());

    let cosmetic = PROJECTOR.replace("  on @thing.done", "\n    on @thing.done");
    write(project.path(), &[("projectors/things.hk", &cosmetic)]);
    assert_eq!(
        forecast(&plan_of(project.path(), data.path()), "Things").outcome,
        Rebuild::Resume,
        "a reformat must not cost a rebuild"
    );

    // A new column is a different read model, so the rows already there were built to
    // a shape that no longer describes them.
    let widened = PROJECTOR
        .replace("shop_id: Int @key,", "shop_id: Int @key,\n    seen: Int,")
        .replace("put Thing { shop_id }", "put Thing { shop_id, seen: 1 }");
    write(project.path(), &[("projectors/things.hk", &widened)]);
    assert_eq!(
        forecast(&plan_of(project.path(), data.path()), "Things").outcome,
        Rebuild::Rebuild
    );
}

/// With `auto_rebuild` off the same change is not a rebuild but a warning: the model
/// keeps serving rows the old logic built until someone replays it.
#[test]
fn auto_rebuild_off_forecasts_stale_rather_than_rebuild() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write(project.path(), &[]);
    fs::write(
        project.path().join("hekla.toml"),
        "[projectors]\nauto_rebuild = false\n",
    )
    .unwrap();
    deploy(project.path(), data.path());

    let widened = PROJECTOR
        .replace("shop_id: Int @key,", "shop_id: Int @key,\n    seen: Int,")
        .replace("put Thing { shop_id }", "put Thing { shop_id, seen: 1 }");
    write(project.path(), &[("projectors/things.hk", &widened)]);
    assert_eq!(
        forecast(&plan_of(project.path(), data.path()), "Things").outcome,
        Rebuild::Stale
    );
}

// --- attribution -------------------------------------------------------------

/// The whole point of grouping: a guard has no digest entry, so editing it moves every
/// caller's hash at once and the report has to say why.
#[test]
fn a_guard_edit_names_the_guard_and_only_its_callers() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write(project.path(), &[]);
    deploy(project.path(), data.path());

    let edited = RULES.replace(
        "on @shop.connected(shop_id) => true",
        "on @thing.done(shop_id) => true",
    );
    write(project.path(), &[("lib/rules.hk", &edited)]);

    let plan = plan_of(project.path(), data.path());
    let guards: Vec<_> = plan
        .causes
        .iter()
        .filter_map(|cause| match cause {
            Cause::Guard { name, commands } => Some((name.as_str(), commands.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(guards.len(), 1, "expected one guard cause: {plan}");
    let (name, mut commands) = guards.into_iter().next().unwrap();
    assert_eq!(name, "ShopIsConnected");
    commands.sort();
    assert_eq!(commands, vec!["DoA".to_owned(), "DoB".to_owned()]);
    // The control. `DoC` names no guard, so it must be untouched: an attribution that
    // swept it in would be describing a coincidence as a cause.
    assert!(change(&plan, "DoC").is_none(), "{plan}");
}

/// A refusal is spliced into the guard that rejects with it, so a reworded message
/// satisfies the guard test too. The more specific cause has to win, or a reader goes
/// looking at the fold when what moved was the string beside it.
#[test]
fn a_reworded_refusal_is_named_rather_than_the_guard_around_it() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write(project.path(), &[]);
    deploy(project.path(), data.path());

    let edited = RULES.replace(
        "\"the shop is not connected\"",
        "\"that shop has not been connected yet\"",
    );
    write(project.path(), &[("lib/rules.hk", &edited)]);

    let plan = plan_of(project.path(), data.path());
    let likely: Vec<_> = plan
        .causes
        .iter()
        .filter_map(|cause| match cause {
            Cause::SharedEdit { likely, .. } => likely.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(
        likely,
        vec!["refusal ShopNotConnected".to_owned()],
        "{plan}"
    );
    // Its code did not change, so no caller can tell: the contract holds.
    assert_eq!(
        change(&plan, "DoA").map(|change| change.verdict),
        Some(Verdict::Behaviour),
        "{plan}"
    );
}

/// A `const` is spliced in as its value, so it has no row of its own and nothing at the
/// use site names it. The report reaches it from the edit instead.
#[test]
fn a_const_edit_is_one_shared_edit_and_the_const_has_no_row() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let limit = "const BUSY: Int = 17\n";
    let uses = |name: &str| {
        format!(
            "command {name}(shop_id: Int) {{
  fold seen: Int = 0
    on @thing.done(shop_id) => seen + 1

  if seen >= BUSY {{
    return reject ShopNotConnected
  }}

  emit @thing.done {{ shop_id }}
}}
"
        )
    };
    let (a, b) = (uses("DoA"), uses("DoB"));
    write(
        project.path(),
        &[
            ("lib/limits.hk", limit),
            ("commands/a.hk", &a),
            ("commands/b.hk", &b),
        ],
    );
    deploy(project.path(), data.path());

    write(
        project.path(),
        &[
            ("lib/limits.hk", "const BUSY: Int = 42\n"),
            ("commands/a.hk", &a),
            ("commands/b.hk", &b),
        ],
    );
    let plan = plan_of(project.path(), data.path());

    assert!(
        !plan.changes.iter().any(|change| change.name == "BUSY"),
        "an inlined const must not get a row of its own: {plan}"
    );
    let shared: Vec<_> = plan
        .causes
        .iter()
        .filter_map(|cause| match cause {
            Cause::SharedEdit {
                declarations,
                likely,
                ..
            } => Some((declarations.clone(), likely.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(shared.len(), 1, "expected one shared edit: {plan}");
    let (mut declarations, likely) = shared.into_iter().next().unwrap();
    declarations.sort();
    assert_eq!(declarations, vec!["DoA".to_owned(), "DoB".to_owned()]);
    assert_eq!(likely, Some("const BUSY".to_owned()));
}

// --- what it must not do ------------------------------------------------------

/// The contract that lets `plan` run against production: it reads, and that is all.
#[test]
fn planning_changes_no_database_in_the_data_directory() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write(project.path(), &[]);
    deploy(project.path(), data.path());

    let before = listing(data.path());
    write(
        project.path(),
        &[("commands/c.hk", &DO_C.replace("seen + 1", "seen + 3"))],
    );
    let plan = plan_of(project.path(), data.path());
    assert!(!plan.is_empty(), "the edit should have been seen");
    assert_eq!(
        before,
        listing(data.path()),
        "planning wrote to the data directory"
    );
}

/// `verify` takes the data-directory lock and so refuses a live directory. `plan` opens
/// no event log, which is what lets it answer the question while the server is up.
#[test]
fn planning_runs_while_a_server_holds_the_directory() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write(project.path(), &[]);
    // Deploy first, so the projector has stamped its definition. A plan taken against a
    // directory a server has only just opened is entitled to say the model is fresh.
    deploy(project.path(), data.path());
    let live = boot(project.path(), data.path());

    let plan = plan::compute_with(&load_ok(project.path()), data.path(), plan::Replay::Off)
        .expect("planning must not need the lock");
    assert!(plan.is_empty(), "{plan}");
    live.shutdown();
}

/// Opening through `OpDb::open` would migrate, which for a reader pointed at a live
/// deployment is a write nothing asked for.
#[test]
fn an_older_schema_is_refused_rather_than_migrated() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write(project.path(), &[]);
    deploy(project.path(), data.path());

    let db = data.path().join("hekla.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.pragma_update(None, "user_version", 5i64).unwrap();
    drop(conn);

    let err = plan::compute_with(&load_ok(project.path()), data.path(), plan::Replay::Off)
        .expect_err("an older schema should be refused");
    let text = format!("{err:#}");
    assert!(text.contains("schema version 5"), "{text}");

    let conn = rusqlite::Connection::open(&db).unwrap();
    let after: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(after, 5, "planning migrated a database it was only reading");
}

#[test]
fn a_directory_that_was_never_deployed_to_is_an_error_not_an_empty_plan() {
    let project = write_project(&files());
    let data = tempfile::tempdir().unwrap();
    let err = plan::compute_with(&load_ok(project.path()), data.path(), plan::Replay::Off)
        .expect_err("nothing is deployed there");
    assert!(
        format!("{err:#}").contains("nothing is deployed"),
        "{err:#}"
    );
}

/// Every file the data directory holds with its size, sorted, so a comparison is
/// order-independent and catches a write as well as an appearance.
///
/// SQLite's own `-wal` and `-shm` sidecars are excluded. Opening a WAL database
/// read-only still maps a shared-memory index, and a read-only connection cannot
/// remove it on close, so planning against a directory whose server is down leaves an
/// empty pair behind. They hold no state of hekla's and the next boot reclaims them;
/// what must not move is the databases themselves.
fn listing(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in fs::read_dir(&next).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                stack.push(path.clone());
            }
            let rel = path.strip_prefix(dir).unwrap().display().to_string();
            if rel.ends_with("-wal") || rel.ends_with("-shm") {
                continue;
            }
            found.push(format!("{rel} {}", entry.metadata().unwrap().len()));
        }
    }
    found.sort();
    found
}

// --- the command itself --------------------------------------------------------

#[test]
fn the_cli_keeps_stdout_machine_readable_and_exits_zero_on_a_change() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write(project.path(), &[]);
    deploy(project.path(), data.path());
    write(
        project.path(),
        &[("commands/c.hk", &DO_C.replace("seen + 1", "seen + 4"))],
    );

    let output = Command::new(env!("CARGO_BIN_EXE_hekla"))
        .arg("plan")
        .arg(project.path())
        .arg("--data-dir")
        .arg(data.path())
        .arg("--json")
        .output()
        .expect("running `hekla plan`");

    // A change is the answer, not a fault: a command that fails when it succeeds is no
    // use in a pipeline.
    assert!(
        output.status.success(),
        "`hekla plan` exited non-zero on a change: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
            panic!(
                "stdout is not pure JSON ({err}); it began: {:?}",
                String::from_utf8_lossy(&output.stdout)
                    .chars()
                    .take(200)
                    .collect::<String>()
            )
        });
    let changes = document["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1, "{document}");
    assert_eq!(changes[0]["name"], "DoC");
    assert_eq!(changes[0]["verdict"], "behaviour");
}

#[test]
fn the_cli_refuses_a_path_that_is_not_a_project() {
    let output = Command::new(env!("CARGO_BIN_EXE_hekla"))
        .arg("plan")
        .arg("does-not-exist")
        .output()
        .expect("running `hekla plan`");
    assert!(!output.status.success(), "a missing directory exited 0");
    assert!(output.stdout.is_empty(), "a stub plan was printed");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("is not a directory"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// --- effect replay ----------------------------------------------------------
//
// A declaration diff says an effect changed. These say whether the change matters,
// which is the question a deploy actually turns on. Each one is the same three steps:
// deploy the fixture and let the effect journal a real invocation, rewrite the source,
// then plan against the untouched directory with replay on.
//
// Same rule as above, and it matters more here: an edit that must move nothing is
// tested beside every edit that must, because a replay that concluded nothing looks
// exactly like one that concluded everything unless something pins the difference.

const ORDER_EVENTS: &str = "\
event @order.placed { order_id: Int, email: String @max(100) }
event @order.cancelled { order_id: Int }
";

const PLACE_ORDER: &str = "\
command PlaceOrder(order_id: Int, email: String) {
  emit @order.placed { order_id, email }
}
";

/// A module-level `fn`, which is a digest entry of its own. Editing it does *not* move
/// the calling effect's hash, which is the false negative `affected_effects` exists to
/// close.
const ENDPOINT: &str = "\
fn endpoint(order_id: Int) -> String {
  return \"https://mail.test/send/{order_id}\"
}
";

const NOTIFY: &str = "\
effect Notify {
  on @order.placed { order_id, email } {
    http.post(endpoint(order_id), { \"to\": email })
  }
}
";

fn effect_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("events/order.hk", ORDER_EVENTS),
        ("commands/place.hk", PLACE_ORDER),
        ("lib/endpoint.hk", ENDPOINT),
        ("effects/notify.hk", NOTIFY),
    ]
}

fn write_effect_project(dir: &Path, overrides: &[(&str, &str)]) {
    let mut written = Vec::new();
    for (rel, content) in effect_files() {
        let content = overrides
            .iter()
            .find(|(path, _)| *path == rel)
            .map(|(_, replacement)| *replacement)
            .unwrap_or(content);
        write_one(dir, rel, content);
        written.push(rel);
    }
    for (rel, content) in overrides {
        if !written.contains(rel) {
            write_one(dir, rel, content);
        }
    }
}

/// Boot the effect fixture, place `orders` orders, wait for the effect to journal every
/// one of them, and shut down. What is left on disk is a deployment with real recorded
/// invocations, which is the baseline every replay below runs against.
fn deploy_with_invocations(project: &Path, data: &Path, orders: u64) {
    record_invocations(project, data, 1, orders);
}

/// The same, for orders `first..=last` against a directory that already holds some.
///
/// One order is one event, so an order's number is also its log position, which is what
/// lets a test name the position it expects a divergence at.
fn record_invocations(project: &Path, data: &Path, first: u64, last: u64) {
    let harness = Boot::new(project)
        .data_dir(data)
        .http(Arc::new(StubHttpClient::ok()))
        .start();
    for index in first..=last {
        let body = json!({ "order_id": index, "email": format!("o{index}@test") });
        let result = harness
            .rt
            .execute("PlaceOrder", body, &ctx(), None)
            .unwrap();
        assert_eq!(result.status, 200, "PlaceOrder failed: {:?}", result.body);
    }
    wait_effect_position(&harness.rt, "Notify", last);
    quiesce(&harness);
    harness.shutdown();
}

/// Replay with the default cap, which every fixture here is far below.
fn replay_options(master: Option<MasterKeys>) -> plan::Replay {
    plan::Replay::On {
        master,
        limit: plan::DEFAULT_REPLAY_LIMIT as usize,
    }
}

/// Plan with replay on, and no master key: the fixture seals nothing to a subject.
fn replay_of(project: &Path, data: &Path) -> plan::Plan {
    plan::compute_with(&load_ok(project), data, replay_options(None))
        .expect("planning should succeed")
}

fn coverage(plan: &plan::Plan) -> &plan::Coverage {
    plan.coverage
        .as_ref()
        .expect("a plan computed with replay on carries coverage")
}

/// The edit most replay tests plan: one extra call, which the recorded journal has no
/// entry for.
fn add_an_audit_call(project: &Path) {
    write_one(
        project,
        "effects/notify.hk",
        "\
effect Notify {
  on @order.placed { order_id, email } {
    http.post(endpoint(order_id), { \"to\": email })
    http.post(\"https://audit.test/log\", { \"order_id\": order_id })
  }
}
",
    );
}

#[test]
fn an_effect_edit_that_changes_no_call_reproduces() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(project.path(), &[]);
    deploy_with_invocations(project.path(), data.path(), 2);

    // `log` is not journaled, so this moves the effect's digest without moving a single
    // call. The declaration diff has to report it and the replay has to clear it, and
    // that gap between the two is the whole reason `--replay` exists.
    write_one(
        project.path(),
        "effects/notify.hk",
        "\
effect Notify {
  on @order.placed { order_id, email } {
    log(\"confirming order {order_id}\")
    http.post(endpoint(order_id), { \"to\": email })
  }
}
",
    );

    let plan = replay_of(project.path(), data.path());
    assert!(
        change(&plan, "Notify").is_some(),
        "the declaration diff still reports the edit"
    );
    assert!(
        plan.divergences.is_empty(),
        "expected no divergence, got {:?}",
        plan.divergences
    );
    let coverage = coverage(&plan);
    assert_eq!(coverage.effects_affected, 1);
    assert_eq!(
        coverage.replayed, 2,
        "both recorded invocations were replayed"
    );
    assert_eq!(coverage.reproduced, 2);
}

#[test]
fn an_effect_that_would_make_a_new_call_is_reported() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(project.path(), &[]);
    deploy_with_invocations(project.path(), data.path(), 2);

    add_an_audit_call(project.path());

    let plan = replay_of(project.path(), data.path());
    assert_eq!(
        plan.divergences.len(),
        2,
        "one per recorded invocation, got {:?}",
        plan.divergences
    );
    for divergence in &plan.divergences {
        assert_eq!(divergence.effect, "Notify");
        assert!(
            matches!(divergence.outcome, Replayed::NewCall { .. }),
            "expected a new call, got {:?}",
            divergence.outcome
        );
    }
    let coverage = coverage(&plan);
    assert_eq!(coverage.replayed, 2);
    assert_eq!(coverage.reproduced, 0);
    assert!(!plan.is_empty());
    // A `plan` reader is being told what the candidate would newly do, not what a retry
    // of the recorded run would have done. The same outcome carries both readings and
    // printing only the retry one told this reader about a double-fire that is not the
    // finding.
    assert!(
        plan.to_string()
            .contains("it reached a call the recorded run never made"),
        "the report names what the call was, as a candidate deploy reads it: {plan}"
    );
}

/// The false negative that decides the design. The effect's own source is untouched, so
/// its digest hash has not moved and a `script_hash` comparison would skip every one of
/// its invocations. The helper it calls is what changed, and the URL it builds with it.
#[test]
fn a_changed_helper_pulls_its_calling_effect_into_the_replay() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(project.path(), &[]);
    deploy_with_invocations(project.path(), data.path(), 1);

    write_one(
        project.path(),
        "lib/endpoint.hk",
        "\
fn endpoint(order_id: Int) -> String {
  return \"https://mail.test/v2/send/{order_id}\"
}
",
    );

    let plan = replay_of(project.path(), data.path());
    assert!(
        change(&plan, "Notify").is_none(),
        "the effect itself did not change, so nothing should say it did"
    );
    assert!(
        change(&plan, "endpoint").is_some(),
        "the helper is a declaration of its own and did change"
    );
    let coverage = coverage(&plan);
    assert_eq!(
        coverage.effects_affected, 1,
        "an effect reached through the call graph is still affected"
    );
    assert_eq!(coverage.replayed, 1);
    assert_eq!(
        plan.divergences.len(),
        1,
        "the new URL is a call the recorded run never made, got {:?}",
        plan.divergences
    );
}

#[test]
fn an_effect_that_no_longer_handles_the_event_is_reported() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(project.path(), &[]);
    deploy_with_invocations(project.path(), data.path(), 1);

    write_one(
        project.path(),
        "effects/notify.hk",
        "\
effect Notify {
  on @order.cancelled { order_id } {
    http.post(endpoint(order_id), { \"to\": \"nobody\" })
  }
}
",
    );

    let plan = replay_of(project.path(), data.path());
    assert_eq!(plan.divergences.len(), 1, "got {:?}", plan.divergences);
    assert!(
        matches!(plan.divergences[0].outcome, Replayed::NoLongerHandled),
        "expected the arm to no longer select it, got {:?}",
        plan.divergences[0].outcome
    );
    assert!(plan.to_string().contains("would not run at all"), "{plan}");
}

#[test]
fn an_unchanged_effect_is_not_replayed() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(project.path(), &[]);
    deploy_with_invocations(project.path(), data.path(), 2);

    // A command edit, which cannot reach the effect.
    write_one(
        project.path(),
        "commands/place.hk",
        "\
command PlaceOrder(order_id: Int, email: String) {
  fold placed: Int = 0
    on @order.placed(order_id) => placed + 1

  emit @order.placed { order_id, email }
}
",
    );

    let plan = replay_of(project.path(), data.path());
    let coverage = coverage(&plan);
    assert_eq!(
        coverage.effects_affected, 0,
        "nothing this deploy touches can move what the effect does"
    );
    assert_eq!(coverage.replayed, 0);
    assert!(plan.divergences.is_empty());
    // Which is exactly why the count above has to be printed: no divergence here means
    // nothing was looked at, and that must not read like a clean replay.
    assert!(coverage.is_blind());
}

/// An event declaration the effect does not mention textually still moves what the
/// effect *does*, so changing one has to pull it into the replay.
///
/// heklang gives an event its own digest entry, and an effect arm binds a field by name:
/// `Frame::trigger` emits the bind's name and slot and never its type. So the whole of
/// "the field the handler posts is now sealed to a subject" happens without a byte of the
/// effect's own hash moving. A check that keyed only on that hash would report this
/// deploy as fully covered and entirely clean.
#[test]
fn an_event_edit_pulls_its_consuming_effect_into_the_replay() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(project.path(), &[]);
    deploy_with_invocations(project.path(), data.path(), 2);

    // Only the event. The command and the effect are byte-identical to what is deployed.
    write_one(
        project.path(),
        "events/order.hk",
        "\
event @order.placed { order_id: Int, email: String @max(200) }
event @order.cancelled { order_id: Int }
",
    );

    let plan = replay_of(project.path(), data.path());
    assert!(
        change(&plan, "@order.placed").is_some(),
        "the diff sees the event: {plan}"
    );
    let coverage = coverage(&plan);
    assert_eq!(
        coverage.effects_affected, 1,
        "the effect handles the changed event, so this deploy could move it"
    );
    assert_eq!(
        coverage.replayed, 2,
        "both recorded invocations were actually re-run"
    );
    assert_eq!(
        coverage.reproduced, 2,
        "this particular edit moves no call, which is a finding the replay had to make"
    );
    assert!(plan.divergences.is_empty(), "{:?}", plan.divergences);
}

/// The baseline is what is *running*, not everything on disk.
///
/// Retention keeps a week of invocations, which outlives an edit, so rows written by a
/// version already replaced are still here. Replaying those against the candidate finds
/// the difference between last week's code and this week's, which the running deployment
/// already has: a true statement about the wrong two programs, reported as a finding
/// about this deploy.
#[test]
fn an_invocation_recorded_by_a_superseded_version_is_not_replayed() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(project.path(), &[]);
    // Two invocations of v1 (one call each).
    deploy_with_invocations(project.path(), data.path(), 2);
    // v2 is deployed and records one more. v1's rows are now history.
    add_an_audit_call(project.path());
    record_invocations(project.path(), data.path(), 3, 3);

    // v3: a third call, which v2 would not make.
    write_one(
        project.path(),
        "effects/notify.hk",
        "\
effect Notify {
  on @order.placed { order_id, email } {
    http.post(endpoint(order_id), { \"to\": email })
    http.post(\"https://audit.test/log\", { \"order_id\": order_id })
    http.post(\"https://audit.test/v3\", { \"order_id\": order_id })
  }
}
",
    );

    let plan = replay_of(project.path(), data.path());
    let coverage = coverage(&plan);
    assert_eq!(
        coverage.replayed, 1,
        "only the invocation the deployed version recorded is a baseline for this deploy"
    );
    assert_eq!(
        plan.divergences.len(),
        1,
        "and it does diverge, so the replay ran: {:?}",
        plan.divergences
    );
    assert_eq!(
        plan.divergences[0].position, 3,
        "the v2 invocation, not either of v1's"
    );
}

/// A plan that ran no replay must not report a replay result.
///
/// Without `--replay` nothing opens the event log, so "0 recorded invocation(s) would
/// diverge" would be a clean check that was never made. The summary line is the part an
/// operator reads, which makes it the worst place to say it.
#[test]
fn a_plan_without_replay_says_nothing_about_divergence() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(project.path(), &[]);
    deploy_with_invocations(project.path(), data.path(), 1);
    add_an_audit_call(project.path());

    let plan = plan_of(project.path(), data.path());
    assert!(plan.coverage.is_none(), "replay was not asked for");
    assert!(
        !plan.is_empty(),
        "the effect edit is a change, so there is a summary"
    );
    let text = plan.to_string();
    assert!(
        !text.contains("diverge"),
        "a plan that opened no log claims nothing about divergence: {text}"
    );

    // And with the replay on, over the same directory, it does say so.
    let replayed = replay_of(project.path(), data.path());
    assert!(
        replayed
            .to_string()
            .contains("1 recorded invocation(s) would diverge"),
        "{replayed}"
    );
}

/// The whole point of the follower. `verify` refuses to run against a live directory
/// because it takes the lock; this must not, because checking a candidate against
/// production without disturbing it is the reason the command exists.
#[test]
fn replay_runs_while_a_server_holds_the_directory() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(project.path(), &[]);
    deploy_with_invocations(project.path(), data.path(), 2);

    let live = Boot::new(project.path())
        .data_dir(data.path())
        .http(Arc::new(StubHttpClient::ok()))
        .start();

    add_an_audit_call(project.path());

    let plan = replay_of(project.path(), data.path());
    assert_eq!(
        plan.divergences.len(),
        2,
        "a live writer must not cost the replay its coverage, got {:?}",
        plan.divergences
    );
    live.shutdown();
}

/// A replay reads the log through read-only descriptors and reads `hekla.db` through a
/// connection that only ever selects. Every byte of the event store has to be where it
/// was: unlike the SQLite files, a segment has no sidecar excuse.
#[test]
fn replay_changes_no_event_segment() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(project.path(), &[]);
    deploy_with_invocations(project.path(), data.path(), 2);

    let events = data.path().join("events");
    let before = segments(&events);
    assert!(!before.is_empty(), "the fixture wrote a segment");

    add_an_audit_call(project.path());
    let plan = replay_of(project.path(), data.path());
    assert!(!plan.divergences.is_empty(), "the replay actually ran");

    assert_eq!(
        segments(&events),
        before,
        "a follower creates, deletes and rewrites nothing"
    );
}

/// Every file under `dir`, recursively, by relative path and content.
///
/// Recursive on purpose: `events/` holds the log segments *and* an `index/` beside them,
/// and a follower rebuilds an index in memory rather than rewriting the `.idx` on disk.
/// A listing that stopped at the top level would miss exactly that.
fn segments(dir: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &Path, prefix: &str, into: &mut Vec<(String, Vec<u8>)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            if entry.file_type().unwrap().is_dir() {
                walk(&entry.path(), &rel, into);
            } else {
                into.push((rel, fs::read(entry.path()).unwrap()));
            }
        }
    }
    let mut found = Vec::new();
    walk(dir, "", &mut found);
    found.sort_by(|(left, _), (right, _)| left.cmp(right));
    found
}

/// A data directory whose log has never been written is not an error. Nothing could
/// have been recorded against a log that does not exist, so the honest answer is no
/// coverage rather than a failure.
#[test]
fn an_uninitialized_log_replays_nothing_rather_than_failing() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(project.path(), &[]);
    deploy_with_invocations(project.path(), data.path(), 1);
    fs::remove_dir_all(data.path().join("events")).unwrap();

    write_one(
        project.path(),
        "effects/notify.hk",
        "\
effect Notify {
  on @order.placed { order_id, email } {
    http.post(\"https://elsewhere.test\", { \"to\": email })
  }
}
",
    );

    let plan = replay_of(project.path(), data.path());
    let coverage = coverage(&plan);
    assert_eq!(coverage.effects_affected, 1);
    assert_eq!(coverage.replayed, 0);
    assert!(plan.divergences.is_empty());
}

#[test]
fn the_cli_carries_the_replay_in_its_json() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(project.path(), &[]);
    deploy_with_invocations(project.path(), data.path(), 1);

    add_an_audit_call(project.path());

    let output = Command::new(env!("CARGO_BIN_EXE_hekla"))
        .arg("plan")
        .arg(project.path())
        .arg("--data-dir")
        .arg(data.path())
        .arg("--replay")
        .arg("--json")
        .output()
        .expect("running hekla plan");
    assert!(
        output.status.success(),
        "a divergence is a report, not a failure: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed: Value =
        serde_json::from_slice(&output.stdout).expect("stdout has to stay machine-readable");
    assert_eq!(parsed["coverage"]["effects_affected"], 1);
    assert_eq!(parsed["coverage"]["replayed"], 1);
    assert_eq!(parsed["coverage"]["reproduced"], 0);
    assert_eq!(parsed["divergences"][0]["effect"], "Notify");
    assert_eq!(parsed["divergences"][0]["outcome"], "new_call");
}

/// Without `--replay` the command keeps its older promise exactly: no log, no key, and
/// no coverage claimed. `None` is not the same as a replay that covered nothing.
#[test]
fn a_plan_without_replay_claims_no_coverage() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(project.path(), &[]);
    deploy_with_invocations(project.path(), data.path(), 1);

    let plan =
        plan::compute_with(&load_ok(project.path()), data.path(), plan::Replay::Off).unwrap();
    assert!(plan.coverage.is_none());
    assert!(plan.divergences.is_empty());
}

// --- what replay cannot see -------------------------------------------------
//
// Both limits below are permanent, and neither is a bug. What would be a bug is
// reporting either as a divergence, or folding either into the coverage as though it
// had been checked.

const SEALED_EVENTS: &str = "\
event @order.placed {
  order_id: Int,
  customer_id: Int,
  email: String? @subject(customer_id) @max(100),
}
";

const SEALED_PLACE: &str = "\
command PlaceOrder(order_id: Int, customer_id: Int, email: String) {
  emit @order.placed { order_id, customer_id, email }
}
";

const SEALED_NOTIFY: &str = "\
effect Notify {
  on @order.placed { order_id, email } {
    http.post(endpoint(order_id), { \"to\": reveal(email) })
  }
}
";

fn sealed_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("events/order.hk", SEALED_EVENTS),
        ("commands/place.hk", SEALED_PLACE),
        ("lib/endpoint.hk", ENDPOINT),
        ("effects/notify.hk", SEALED_NOTIFY),
    ]
}

/// Deploy the sealed fixture with a master key and let the effect journal one call.
fn deploy_sealed(project: &Path, data: &Path) {
    deploy_sealed_orders(project, data, 1);
}

/// The same, for `orders` of them, when a test needs more history than the cap it sets.
fn deploy_sealed_orders(project: &Path, data: &Path, orders: u64) {
    for (rel, content) in sealed_files() {
        write_one(project, rel, content);
    }
    let harness = Boot::new(project)
        .data_dir(data)
        .with_master_key()
        .http(Arc::new(StubHttpClient::ok()))
        .start();
    for index in 1..=orders {
        let body = json!({ "order_id": index, "customer_id": 7, "email": "a@test" });
        let result = harness
            .rt
            .execute("PlaceOrder", body, &ctx(), None)
            .unwrap();
        assert_eq!(result.status, 200, "PlaceOrder failed: {:?}", result.body);
    }
    wait_effect_position(&harness.rt, "Notify", orders);
    quiesce(&harness);
    harness.shutdown();
}

/// The edit every test below plans: a second call, which would diverge loudly if the
/// invocation could be replayed at all.
fn add_a_second_call(project: &Path) {
    write_one(
        project,
        "effects/notify.hk",
        "\
effect Notify {
  on @order.placed { order_id, email } {
    http.post(endpoint(order_id), { \"to\": reveal(email) })
    http.post(\"https://audit.test/log\", { \"order_id\": order_id })
  }
}
",
    );
}

/// An erased subject cannot be replayed, and that is by design rather than by accident:
/// the plaintext the handler branches on is gone, and journaling it to make this
/// answerable would defeat the erasure it was destroyed for. What matters is that it
/// reads as uncovered rather than as either a pass or a divergence.
#[test]
fn an_erased_subject_is_unreplayable_rather_than_a_divergence() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    deploy_sealed(project.path(), data.path());

    let opdb = hekla::opdb::OpDb::open(&data.path().join("hekla.db")).unwrap();
    assert!(
        hekla::crypto::erase_subject(&opdb, "customer_id", "7").unwrap(),
        "the fixture wrote a key to erase"
    );
    drop(opdb);

    add_a_second_call(project.path());
    let plan = plan::compute_with(
        &load_ok(project.path()),
        data.path(),
        replay_options(Some(support::master_keys())),
    )
    .unwrap();

    assert!(
        plan.divergences.is_empty(),
        "an unanswerable invocation is not a divergence, got {:?}",
        plan.divergences
    );
    let coverage = coverage(&plan);
    assert_eq!(coverage.effects_affected, 1);
    assert_eq!(coverage.replayed, 0, "nothing could be concluded");
    assert_eq!(coverage.subject_erased, 1);
    assert!(
        plan.to_string().contains("subject has been erased"),
        "the report has to say what it could not cover: {plan}"
    );
}

/// A CI job planning against production should not need the production master key to
/// see the declaration diff, so a missing one degrades rather than refuses. It is
/// counted once, up front: letting it reach the replay would report one divergence per
/// invocation for a key that was never configured.
#[test]
fn a_sealed_project_without_a_master_key_reports_no_coverage() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    deploy_sealed(project.path(), data.path());
    add_a_second_call(project.path());

    let plan =
        plan::compute_with(&load_ok(project.path()), data.path(), replay_options(None)).unwrap();

    assert!(
        change(&plan, "Notify").is_some(),
        "the diff still works without a key"
    );
    assert!(
        plan.divergences.is_empty(),
        "a missing key is a gap in coverage, not a finding, got {:?}",
        plan.divergences
    );
    let coverage = coverage(&plan);
    assert_eq!(coverage.replayed, 0);
    assert_eq!(coverage.no_master_key, 1);
    assert!(
        plan.to_string().contains("HEKLA_MASTER_KEY is not set"),
        "{plan}"
    );
}

/// The affected set is what a `no_master_key` count applies to, and it applies per
/// effect. One sealed field anywhere used to zero the coverage of every effect in the
/// project, including the ones that never touch a key.
#[test]
fn only_an_effect_that_reveals_needs_the_master_key() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    for (rel, content) in sealed_files() {
        write_one(project.path(), rel, content);
    }
    // A second effect on the same event that never reveals: it reads the plaintext id.
    write_one(
        project.path(),
        "effects/audit.hk",
        "\
effect Audit {
  on @order.placed { order_id } {
    http.post(\"https://audit.test/log\", { \"order_id\": order_id })
  }
}
",
    );
    {
        let harness = Boot::new(project.path())
            .data_dir(data.path())
            .with_master_key()
            .http(Arc::new(StubHttpClient::ok()))
            .start();
        let body = json!({ "order_id": 1, "customer_id": 7, "email": "a@test" });
        harness
            .rt
            .execute("PlaceOrder", body, &ctx(), None)
            .unwrap();
        wait_effect_position(&harness.rt, "Notify", 1);
        wait_effect_position(&harness.rt, "Audit", 1);
        quiesce(&harness);
        harness.shutdown();
    }

    // Both effects gain a call, so both are affected. Only `Notify` reveals.
    add_a_second_call(project.path());
    write_one(
        project.path(),
        "effects/audit.hk",
        "\
effect Audit {
  on @order.placed { order_id } {
    http.post(\"https://audit.test/log\", { \"order_id\": order_id })
    http.post(\"https://audit.test/v2\", { \"order_id\": order_id })
  }
}
",
    );

    let plan =
        plan::compute_with(&load_ok(project.path()), data.path(), replay_options(None)).unwrap();
    let coverage = coverage(&plan);
    assert_eq!(coverage.effects_affected, 2);
    assert_eq!(
        coverage.no_master_key, 1,
        "only `Notify` reveals, so only its invocation is out of reach"
    );
    assert_eq!(
        coverage.replayed, 1,
        "`Audit` never needed a key and must still be covered"
    );
    assert_eq!(
        plan.divergences.len(),
        1,
        "and its new call is still found, got {:?}",
        plan.divergences
    );
    assert_eq!(plan.divergences[0].effect, "Audit");
}

/// The cap has to be visible. A report that quietly replayed the first N of a long
/// history reads exactly like one that replayed all of it.
#[test]
fn the_replay_limit_names_what_it_dropped() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(project.path(), &[]);
    deploy_with_invocations(project.path(), data.path(), 3);
    add_an_audit_call(project.path());

    let plan = plan::compute_with(
        &load_ok(project.path()),
        data.path(),
        plan::Replay::On {
            master: None,
            limit: 2,
        },
    )
    .unwrap();
    let coverage = coverage(&plan);
    assert_eq!(coverage.replayed, 2, "the cap held");
    assert_eq!(
        coverage.truncated,
        vec!["Notify".to_string()],
        "and said which effect it held for"
    );
    assert!(
        plan.to_string().contains("capped at the 2 most recent"),
        "{plan}"
    );
}

/// An invocation that journaled nothing and an arm that no longer selects the event are
/// both empty on the replay side, so the comparison alone cannot tell them apart. The
/// news is that the effect would stop firing, and it must not be buried as a match.
#[test]
fn an_effect_that_stops_handling_an_uncalled_invocation_is_still_reported() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    // An arm that calls nothing at all, so the recorded journal is empty.
    write_effect_project(
        project.path(),
        &[(
            "effects/notify.hk",
            "\
effect Notify {
  on @order.placed { order_id } {
    log(\"seen {order_id}\")
  }
}
",
        )],
    );
    deploy_with_invocations(project.path(), data.path(), 1);

    write_one(
        project.path(),
        "effects/notify.hk",
        "\
effect Notify {
  on @order.cancelled { order_id } {
    log(\"seen {order_id}\")
  }
}
",
    );

    let plan = replay_of(project.path(), data.path());
    assert_eq!(plan.divergences.len(), 1, "got {:?}", plan.divergences);
    assert!(
        matches!(plan.divergences[0].outcome, Replayed::NoLongerHandled),
        "an empty journal must not hide it, got {:?}",
        plan.divergences[0].outcome
    );
}

/// A deployment recorded under another digest version has nothing comparable in it, so
/// there is no honest affected set to replay. That is coverage of zero, not the absence
/// of a question: `None` means the caller never asked, and a gate keying on it must not
/// read a refusal as a run it did not request.
#[test]
fn a_digest_version_mismatch_reports_zero_coverage_rather_than_none() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(project.path(), &[]);
    deploy_with_invocations(project.path(), data.path(), 1);

    // Every recorded form now fails to reproduce its own hash, which is what a row
    // written under a different `heklang::digest::VERSION` looks like from here.
    let conn = rusqlite::Connection::open(data.path().join("hekla.db")).unwrap();
    conn.execute("UPDATE declaration SET hash = 'deadbeef' || hash", [])
        .unwrap();
    drop(conn);

    let plan = replay_of(project.path(), data.path());
    assert!(plan.digest_version_mismatch, "the fixture set this up");
    let coverage = coverage(&plan);
    assert_eq!(coverage.effects_affected, 0);
    assert_eq!(coverage.replayed, 0);
    assert!(plan.divergences.is_empty());
}

/// A cap on how much would be replayed says nothing about how much went unreplayed for
/// some other reason, so an effect that was skipped entirely must not also be reported
/// as having had its history capped.
///
/// The count is the real one too. `no_master_key` answers "how much of this deployment
/// could this run not speak for", and clamping that to the replay cap would answer a
/// question nobody asked with a number that looks like the answer to this one.
#[test]
fn an_effect_that_is_not_replayed_is_not_also_reported_as_capped() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    deploy_sealed_orders(project.path(), data.path(), 3);
    add_a_second_call(project.path());

    // A cap of one, which the history is comfortably over, and no key to replay with.
    let plan = plan::compute_with(
        &load_ok(project.path()),
        data.path(),
        plan::Replay::On {
            master: None,
            limit: 1,
        },
    )
    .unwrap();

    let coverage = coverage(&plan);
    assert_eq!(
        coverage.no_master_key, 3,
        "every recorded invocation went unreplayed, and all three are worth saying"
    );
    assert!(
        coverage.truncated.is_empty(),
        "nothing was capped, because nothing was replayed: {:?}",
        coverage.truncated
    );
    assert!(!plan.to_string().contains("capped at"), "{plan}");
}

/// A master key that cannot unwrap what is stored costs the replay, and nothing else.
///
/// A rotation half-configured (the current master set, the previous one forgotten) used
/// to abort the whole command, which threw away a declaration diff and a projector
/// forecast that were already computed and still true. That made a wrong key strictly
/// worse than no key, when the two cost exactly the same thing.
#[test]
fn a_master_key_that_cannot_unwrap_degrades_rather_than_failing() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    deploy_sealed(project.path(), data.path());
    add_a_second_call(project.path());

    let wrong = MasterKeys::new([0x99; 32], vec![]);
    let plan = plan::compute_with(
        &load_ok(project.path()),
        data.path(),
        replay_options(Some(wrong)),
    )
    .expect("a key it cannot use is not a reason to refuse to plan");

    assert!(
        change(&plan, "Notify").is_some(),
        "the diff is what survives, and it has to: {plan}"
    );
    let coverage = coverage(&plan);
    assert_eq!(coverage.no_master_key, 1);
    assert_eq!(coverage.replayed, 0);
    assert!(plan.divergences.is_empty());
    assert!(
        coverage.unusable_master_key.is_some(),
        "and the reader is told why, rather than left to read it as an absent key"
    );
    assert!(
        plan.to_string().contains("cannot unwrap what is stored"),
        "{plan}"
    );
}

/// A cap of zero replays nothing while reporting every effect as capped, which is a
/// coverage number that describes no work at all. Refused at the edge, where the operator
/// can still be told.
#[test]
fn the_replay_limit_refuses_a_cap_of_zero() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(project.path(), &[]);
    deploy_with_invocations(project.path(), data.path(), 1);

    let output = Command::new(env!("CARGO_BIN_EXE_hekla"))
        .args(["plan", "--replay", "--replay-limit", "0"])
        .arg(project.path())
        .arg("--data-dir")
        .arg(data.path())
        .output()
        .unwrap();
    assert!(!output.status.success(), "a cap of zero is not a plan");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("replay-limit"),
        "the refusal names the flag: {stderr}"
    );
}

/// A run that called nothing and a candidate that crashes are two different silences.
///
/// The empty journal is genuinely ambiguous about *calls*: an operator skip leaves the
/// same row a callless run does, so a call the candidate reaches there proves nothing. An
/// error proves something anyway. This code would blow up on this event whatever the
/// recorded run did, and the record neither explains that nor excuses it, so it is a
/// finding rather than a gap in coverage.
#[test]
fn an_invocation_that_journaled_nothing_and_now_crashes_is_reported() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    // An effect that only logs: `log` is not journaled, so every invocation completes
    // `terminal` with an empty journal.
    write_effect_project(
        project.path(),
        &[(
            "effects/notify.hk",
            "\
effect Notify {
  on @order.placed { order_id } {
    log(\"placed {order_id}\")
  }
}
",
        )],
    );
    deploy_with_invocations(project.path(), data.path(), 2);

    // The candidate divides by a value that is only zero at run time, so it errors
    // before it could have reached any call at all.
    write_one(
        project.path(),
        "effects/notify.hk",
        "\
effect Notify {
  on @order.placed { order_id } {
    log(\"placed {order_id / (order_id - order_id)}\")
  }
}
",
    );

    let plan = replay_of(project.path(), data.path());
    let coverage = coverage(&plan);
    assert_eq!(
        coverage.no_journal, 0,
        "an error is not the journal failing to answer: {plan}"
    );
    assert_eq!(
        coverage.replayed, 2,
        "both invocations reached a conclusion"
    );
    assert_eq!(coverage.reproduced, 0);
    assert_eq!(plan.divergences.len(), 2, "{:?}", plan.divergences);
    assert!(
        matches!(plan.divergences[0].outcome, Replayed::Failed { .. }),
        "expected a failure, got {:?}",
        plan.divergences[0].outcome
    );
    assert!(
        plan.to_string().contains("failed part-way through"),
        "{plan}"
    );
}

/// An effect depends on an event two ways, and the digest spells them differently.
///
/// `(events @p ...)` is the arm's trigger list; a `fold` reads through `(slice @p ...)`.
/// Both are the effect reading that event, and reading only the first leaves an effect
/// blind to a change in the one thing its fold is counting.
#[test]
fn an_event_an_effect_only_folds_over_still_pulls_it_in() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(
        project.path(),
        &[(
            "effects/notify.hk",
            "\
effect Notify {
  on @order.placed as e {
    fold cancelled: Int = 0
      on @order.cancelled(order_id: e.order_id) => cancelled + 1

    http.post(endpoint(e.order_id), { \"to\": e.email, \"cancelled\": cancelled })
  }
}
",
        )],
    );
    deploy_with_invocations(project.path(), data.path(), 2);

    // The folded-over event, and nothing else. The arm does not name it in its trigger
    // list, so only the slice connects the two.
    write_one(
        project.path(),
        "events/order.hk",
        "\
event @order.placed { order_id: Int, email: String @max(100) }
event @order.cancelled { order_id: Int, reason: String }
",
    );

    let plan = replay_of(project.path(), data.path());
    assert!(change(&plan, "@order.cancelled").is_some(), "{plan}");
    let coverage = coverage(&plan);
    assert_eq!(
        coverage.effects_affected, 1,
        "the effect folds over the changed event, so this deploy could move it"
    );
    assert_eq!(coverage.replayed, 2);
}

/// A record is named one way in a type and another way in a value, and an effect that
/// only ever *constructs* one never writes the type.
///
/// `(Record N)` appears where a declaration annotates a type; a literal packs as
/// `(new N (f ...) ...)`. An effect body that builds a payload and posts it names the
/// record exclusively through the second, so reading types alone would report a deploy
/// that reshapes that payload as touching nothing.
#[test]
fn a_record_an_effect_only_constructs_still_pulls_it_in() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(
        project.path(),
        &[
            ("lib/payload.hk", "record Payload {\n  to: String,\n}\n"),
            (
                "effects/notify.hk",
                "\
effect Notify {
  on @order.placed { order_id, email } {
    let body = Payload { to: email }
    http.post(endpoint(order_id), { \"to\": body.to })
  }
}
",
            ),
        ],
    );
    deploy_with_invocations(project.path(), data.path(), 2);

    // The record, and nothing else. Wider rather than narrower, so nothing the fixture
    // already wrote could stop fitting.
    write_one(
        project.path(),
        "lib/payload.hk",
        "record Payload {\n  to: String @max(200),\n}\n",
    );

    let plan = replay_of(project.path(), data.path());
    assert!(change(&plan, "Payload").is_some(), "{plan}");
    let coverage = coverage(&plan);
    assert_eq!(
        coverage.effects_affected, 1,
        "the effect builds the changed record, so this deploy could move it"
    );
    assert_eq!(coverage.replayed, 2);
}

/// An effect this deploy *adds* has no deployed version and so no history. Counting it
/// among the affected would report half the affected surface as unreplayed when only
/// half of it could ever have been replayed at all.
#[test]
fn an_added_effect_is_not_counted_against_the_replay() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(project.path(), &[]);
    deploy_with_invocations(project.path(), data.path(), 2);

    add_an_audit_call(project.path());
    write_one(
        project.path(),
        "effects/ship.hk",
        "\
effect Ship {
  on @order.placed { order_id } {
    http.post(\"https://ship.test/queue\", { \"order_id\": order_id })
  }
}
",
    );

    let plan = replay_of(project.path(), data.path());
    assert!(
        change(&plan, "Ship").is_some(),
        "the diff still reports it: {plan}"
    );
    let coverage = coverage(&plan);
    assert_eq!(
        coverage.effects_affected, 1,
        "only the deployed effect has a baseline this deploy could move it away from"
    );
    assert_eq!(coverage.replayed, 2);
}

/// `fail(...)` is rule 4's terminal outcome rather than an error, and the `terminal` row
/// it leaves is the one a success leaves, so nothing on disk says which happened. That
/// makes it news exactly when the program being replayed is not the one that wrote the
/// row: a candidate that would newly give up on recorded events is the finding, and the
/// deployed program failing where it failed is the record reproducing itself.
#[test]
fn a_candidate_that_would_now_fail_terminally_is_reported() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(project.path(), &[]);
    deploy_with_invocations(project.path(), data.path(), 2);

    let giving_up = "\
effect Notify {
  on @order.placed { order_id, email } {
    http.post(endpoint(order_id), { \"to\": email })
    fail(\"the mailer is being retired\")
  }
}
";
    write_one(project.path(), "effects/notify.hk", giving_up);

    let plan = replay_of(project.path(), data.path());
    assert_eq!(plan.divergences.len(), 2, "{:?}", plan.divergences);
    assert!(
        matches!(
            plan.divergences[0].outcome,
            Replayed::TerminallyFailed { .. }
        ),
        "expected a terminal failure, got {:?}",
        plan.divergences[0].outcome
    );
    assert_eq!(coverage(&plan).replayed, 2, "both reached a conclusion");
    assert!(plan.to_string().contains("terminal `fail`"), "{plan}");

    // And once it *is* the deployed program, replaying it against its own journal is a
    // reproduction: it fails where the record says it failed, and the calls it made on
    // the way are what the sweep is checking.
    record_invocations(project.path(), data.path(), 3, 3);
    let report = hekla::verify::sweep(&load_ok(project.path()), data.path(), None).unwrap();
    assert!(report.is_clean(), "{:?}", report.violations);
    assert!(
        report.invocations_checked > 0,
        "a handler that gives up is still a handler whose calls can be compared: {report}"
    );
}

/// A gate reads `--json`, so the surface it reads must not claim a replay result for a
/// run that never opened the log. `null` rather than `[]`, for the reason the summary
/// line prints no divergence clause there.
#[test]
fn the_json_claims_no_divergences_when_no_replay_ran() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(project.path(), &[]);
    deploy_with_invocations(project.path(), data.path(), 1);
    add_an_audit_call(project.path());

    let plan = plan_of(project.path(), data.path());
    let json = plan.json();
    assert!(
        json["divergences"].is_null(),
        "an empty list is a clean replay result: {json}"
    );
    assert!(json["coverage"].is_null());

    let replayed = replay_of(project.path(), data.path());
    let json = replayed.json();
    assert_eq!(
        json["divergences"].as_array().map(Vec::len),
        Some(1),
        "and with the replay on it is a list: {json}"
    );
}

/// A cap on a replay that is not happening is a request this command would otherwise
/// accept and drop, which is the one thing the flag's own contract rules out.
#[test]
fn the_replay_limit_refuses_to_be_set_without_a_replay() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(project.path(), &[]);
    deploy_with_invocations(project.path(), data.path(), 1);

    let output = Command::new(env!("CARGO_BIN_EXE_hekla"))
        .args(["plan", "--replay-limit", "50"])
        .arg(project.path())
        .arg("--data-dir")
        .arg(data.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--replay"), "{stderr}");
}

/// An operator skip leaves a journal that is the *prefix* of a run that never finished,
/// and every reader that inferred "was this skipped?" from the shape of that prefix got
/// it wrong somewhere: an empty one read as a run that called nothing, a partial one read
/// as a complete record to compare against. Both reported a divergence for a directory an
/// operator had deliberately made healthy.
///
/// The row records the skip now, so no shape of journal decides anything. This fixture is
/// deliberately the two-call effect: the one-call effect the first version of this test
/// used cannot produce a partial journal, which is exactly how the partial case survived.
#[test]
fn an_operator_skip_is_uncovered_whatever_its_journal_holds() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write_effect_project(project.path(), &[]);
    add_an_audit_call(project.path());

    {
        // The first call answers and is journaled; the second never does, so the
        // invocation wedges holding half a record of itself.
        let http = Arc::new(StubHttpClient::new(|index, _| {
            if index == 0 {
                Ok(HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: b"{}".to_vec(),
                })
            } else {
                anyhow::bail!("the audit endpoint is unreachable")
            }
        }));
        let harness = Boot::new(project.path())
            .data_dir(data.path())
            .http(http)
            .start();
        let body = json!({ "order_id": 1, "email": "o1@test" });
        let result = harness
            .rt
            .execute("PlaceOrder", body, &ctx(), None)
            .unwrap();
        assert_eq!(result.status, 200, "PlaceOrder failed: {:?}", result.body);
        wait_until("the effect to wedge on its second call", || {
            harness.rt.effect("Notify").unwrap().consecutive_failures() > 0
        });
        harness.rt.effect("Notify").unwrap().request_skip(1);
        wait_until("the skip to advance the effect", || {
            let effect = harness.rt.effect("Notify").unwrap();
            effect.consecutive_failures() == 0 && effect.position() >= 1
        });
        harness.shutdown();
    }

    // `verify` first: the same program over its own record. Before the row carried the
    // skip this reached the unjournaled second call, called it a `NewCall`, and exited
    // non-zero.
    let report = support::sweep(project.path(), data.path());
    assert!(
        report.is_clean(),
        "a skipped invocation is not a violation, got {:?}",
        report.violations
    );
    assert_eq!(report.invocations_checked, 0, "report: {report}");
    assert_eq!(report.skipped.operator_skipped, 1, "report: {report}");
    assert_eq!(
        report.skipped.no_journal, 0,
        "the row answers outright, so nothing falls back to guessing from the journal"
    );

    // And `plan`. `log` is not journaled, so this moves the effect's digest without
    // moving a call: enough to make it affected and replayed, and a candidate the record
    // would otherwise have to be compared against.
    write_one(
        project.path(),
        "effects/notify.hk",
        "\
effect Notify {
  on @order.placed { order_id, email } {
    log(\"notifying {order_id}\")
    http.post(endpoint(order_id), { \"to\": email })
    http.post(\"https://audit.test/log\", { \"order_id\": order_id })
  }
}
",
    );
    let plan =
        plan::compute_with(&load_ok(project.path()), data.path(), replay_options(None)).unwrap();
    assert_eq!(
        plan.coverage.as_ref().map(|c| c.effects_affected),
        Some(1),
        "the edit has to reach the effect for the replay to mean anything"
    );
    let coverage = plan.coverage.expect("a replay was asked for");
    assert_eq!(coverage.operator_skipped, 1);
    assert_eq!(coverage.replayed, 0, "nothing was compared");
    assert_eq!(
        coverage.reproduced, 0,
        "and nothing may be called reproduced"
    );
    assert!(plan.divergences.is_empty(), "got {:?}", plan.divergences);
}
