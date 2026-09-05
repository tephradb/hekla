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

use hekla::plan::{self, Cause, Rebuild, Verdict};
use heklang::Kind;
use support::{Boot, Harness, load_ok, write_project};

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
    plan::compute(&load_ok(project), data).expect("planning should succeed")
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

    let plan = plan::compute(&load_ok(project.path()), data.path())
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

    let err = plan::compute(&load_ok(project.path()), data.path())
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
    let err = plan::compute(&load_ok(project.path()), data.path())
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
