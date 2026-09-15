//! `hekla project`: an undeployed projector folded over a deployed log.
//!
//! Every case here is a deployed directory plus a file that was never deployed, because
//! that is the only shape that catches a projection which is right about a fresh log and
//! wrong about a live one. The properties worth the most are the ones about what it does
//! *not* do: take a lock, write to the data directory, resurrect an erased subject, or
//! let a bounded run read like a complete one.

mod support;

use std::fs;
use std::path::Path;
use std::process::Command;

use hekla::loader::{LoadedProject, Scratch};
use hekla::projection::{self, Request};
use hekla::projector::Stopped;
use serde_json::Value;
use support::{
    ALICE, BOB, Boot, CAROL, boot_example_at, master_keys, orders_project, place_order,
    register_user,
};

// --- fixtures --------------------------------------------------------------

/// Counts registrations by name, so several events fold into one row and the count is
/// visibly a fold rather than a copy.
const BY_NAME: &str = "\
projector ByName {
  entity NameCount {
    name: String @key @max(120),
    registrations: Int,
  }

  on @user.registered { name } {
    patch NameCount[name] { registrations: .registrations + 1 }
  }
}
";

/// Stores the customer's sealed `email` in a column of a *different* name, which is the
/// case a sink carrying the log's ciphertext through verbatim would get wrong: the key
/// store binds the field name into the ciphertext.
const BY_CUSTOMER: &str = "\
projector ByCustomer {
  entity PerCustomer {
    customer_id: Int @key,
    orders: Int,
    last_email: String? @max(100),
  }

  on @order.placed { customer_id, email } {
    patch PerCustomer[customer_id] { orders: .orders + 1, last_email: email }
  }
}
";

const SCRATCH: &str = "question.hk";

/// A request carrying the fixture master key, for the projections that seal a column.
///
/// The key is an input rather than something the library reads off the process, so a
/// sealed projection is testable in-process and so is its absence.
fn sealed_request() -> Request<'static> {
    Request {
        master: Some(master_keys()),
        ..Request::default()
    }
}

fn load(dir: &Path, source: &str) -> LoadedProject {
    LoadedProject::load_with(
        dir,
        Some(Scratch {
            name: SCRATCH,
            source,
        }),
    )
}

/// Fold `source` over `data_dir` with everything at its default.
fn project(dir: &Path, data_dir: &Path, source: &str) -> projection::Projection {
    project_with(dir, data_dir, source, Request::default())
}

fn project_with(
    dir: &Path,
    data_dir: &Path,
    source: &str,
    request: Request<'_>,
) -> projection::Projection {
    let project = load(dir, source);
    assert!(!project.has_errors(), "{:?}", project.findings);
    projection::run(&project, data_dir, &request, &mut |_, _, _| {}).expect("the projection ran")
}

/// One entity's rows, by name.
fn rows<'a>(projection: &'a projection::Projection, entity: &str) -> &'a [Value] {
    &projection
        .entities
        .iter()
        .find(|one| one.name == entity)
        .unwrap_or_else(|| panic!("no entity `{entity}`"))
        .rows
}

// --- the fold --------------------------------------------------------------

#[test]
fn a_scratch_projector_folds_the_deployed_log() {
    let data = tempfile::tempdir().unwrap();
    let harness = boot_example_at(data.path());
    register_user(&harness.rt, ALICE, "alice@example.com", "Ada");
    register_user(&harness.rt, BOB, "bob@example.com", "Ada");
    register_user(&harness.rt, CAROL, "carol@example.com", "Grace");
    harness.shutdown();

    let dir = support::example_dir("users");
    let projection = project(&dir, data.path(), BY_NAME);

    assert_eq!(projection.projector, "ByName");
    assert_eq!(projection.events, 3);
    assert_eq!(projection.stopped, None);
    let rows = rows(&projection, "NameCount");
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0]["name"], "Ada");
    assert_eq!(rows[0]["registrations"], 2);
    assert_eq!(rows[1]["name"], "Grace");
    assert_eq!(rows[1]["registrations"], 1);
}

/// The whole point of compiling the scratch file with the project: it names events it
/// does not declare, and a helper the project does.
#[test]
fn a_scratch_projector_typechecks_against_the_deployed_events() {
    let dir = support::example_dir("users");
    let project = load(
        &dir,
        "\
projector Bad {
  entity Thing { id: String @key @max(20) }
  on @user.nonexistent { id } { put Thing { id } }
}
",
    );
    let errors = support::errors(&project);
    assert!(
        errors.iter().any(|err| err.contains("user.nonexistent")),
        "{errors:?}"
    );
}

/// A follower reads a fixed prefix, so the answer is a snapshot at a position the report
/// names rather than "roughly now".
#[test]
fn a_projection_reports_the_tip_it_was_bounded_by() {
    let data = tempfile::tempdir().unwrap();
    let dir = support::example_dir("users");

    let harness = boot_example_at(data.path());
    register_user(&harness.rt, ALICE, "alice@example.com", "Ada");
    harness.shutdown();
    let first = project(&dir, data.path(), BY_NAME);
    assert_eq!(first.position, 1);
    assert_eq!(first.head, 1);

    let harness = boot_example_at(data.path());
    register_user(&harness.rt, BOB, "bob@example.com", "Ada");
    harness.shutdown();
    let second = project(&dir, data.path(), BY_NAME);
    assert_eq!(second.position, 2, "a second fold sees the larger log");
    assert_eq!(second.events, 2);
}

/// A run that spent its budget answered a different question than one that finished, so
/// nothing about it may read like a complete answer.
#[test]
fn a_budget_that_stops_the_fold_says_so() {
    let data = tempfile::tempdir().unwrap();
    let harness = boot_example_at(data.path());
    // Distinct emails, shared names: the projector folds on the name, and `RegisterUser`
    // refuses a repeated address.
    register_user(&harness.rt, ALICE, "alice@example.com", "Ada");
    register_user(&harness.rt, BOB, "bob@example.com", "Ada");
    register_user(&harness.rt, CAROL, "carol@example.com", "Grace");
    harness.shutdown();

    let dir = support::example_dir("users");
    let projection = project_with(
        &dir,
        data.path(),
        BY_NAME,
        Request {
            max_events: Some(2),
            ..Request::default()
        },
    );

    assert_eq!(projection.stopped, Some(Stopped::MaxEvents));
    assert_eq!(projection.events, 2);
    assert_eq!(rows(&projection, "NameCount").len(), 1, "only Ada");
    let report = projection.to_string();
    assert!(report.starts_with("folded 2 event(s)"), "{report}");
    assert!(report.contains("partial:"), "{report}");
    assert!(!report.contains("ok:"), "{report}");
    assert_eq!(projection.json()["scanned"]["stopped"], "max-events");
}

/// The boundary between the two, and the one that is easy to get backwards: a budget
/// that exactly covers the matching events left nothing unread, so it is a complete
/// answer. tephra reports its cap as hit when the cap and the range run out together, so
/// reading "was the budget spent" off the reader would call this run truncated.
#[test]
fn a_budget_that_exactly_covers_the_log_is_not_partial() {
    let data = tempfile::tempdir().unwrap();
    let harness = boot_example_at(data.path());
    register_user(&harness.rt, ALICE, "alice@example.com", "Ada");
    register_user(&harness.rt, BOB, "bob@example.com", "Ada");
    register_user(&harness.rt, CAROL, "carol@example.com", "Grace");
    harness.shutdown();

    let dir = support::example_dir("users");
    let projection = project_with(
        &dir,
        data.path(),
        BY_NAME,
        Request {
            max_events: Some(3),
            ..Request::default()
        },
    );

    assert_eq!(projection.stopped, None, "the window was covered");
    assert_eq!(projection.events, 3);
    assert_eq!(projection.position, projection.head, "it reached the tip");
    let report = projection.to_string();
    assert!(report.contains("ok: 2 row(s) from 3 event(s)"), "{report}");
    assert!(!report.contains("partial:"), "{report}");
}

/// A window that cannot hold anything is a typo, and answering it would answer a
/// question nobody asked with a clean `ok: 0 row(s)`.
#[test]
fn a_window_that_holds_nothing_is_refused() {
    let data = tempfile::tempdir().unwrap();
    let harness = boot_example_at(data.path());
    register_user(&harness.rt, ALICE, "alice@example.com", "Ada");
    harness.shutdown();

    let dir = support::example_dir("users");
    let project = load(&dir, BY_NAME);

    let above = Request {
        from: Some(100),
        ..Request::default()
    };
    let Err(err) = projection::run(&project, data.path(), &above, &mut |_, _, _| {}) else {
        panic!("a window above the log holds nothing")
    };
    assert!(format!("{err:#}").contains("the whole log"), "{err:#}");

    let inverted = Request {
        from: Some(5),
        upto: Some(2),
        ..Request::default()
    };
    let Err(err) = projection::run(&project, data.path(), &inverted, &mut |_, _, _| {}) else {
        panic!("a window whose start is above its end holds nothing")
    };
    assert!(format!("{err:#}").contains("is above --upto"), "{err:#}");
}

/// The control for the case above: a fold that covered its window says so, and says it
/// differently.
#[test]
fn a_fold_that_covered_its_window_reads_as_complete() {
    let data = tempfile::tempdir().unwrap();
    let harness = boot_example_at(data.path());
    register_user(&harness.rt, ALICE, "alice@example.com", "Ada");
    harness.shutdown();

    let dir = support::example_dir("users");
    let projection = project(&dir, data.path(), BY_NAME);
    let report = projection.to_string();
    assert!(report.contains("ok: 1 row(s) from 1 event(s)"), "{report}");
    assert!(report.contains("a snapshot at position 1"), "{report}");
    assert_eq!(projection.json()["scanned"]["stopped"], Value::Null);
}

#[test]
fn a_window_bounds_the_fold() {
    let data = tempfile::tempdir().unwrap();
    let harness = boot_example_at(data.path());
    register_user(&harness.rt, ALICE, "alice@example.com", "Ada");
    register_user(&harness.rt, BOB, "bob@example.com", "Grace");
    harness.shutdown();

    let dir = support::example_dir("users");
    let projection = project_with(
        &dir,
        data.path(),
        BY_NAME,
        Request {
            from: Some(2),
            ..Request::default()
        },
    );
    assert_eq!(projection.events, 1);
    let rows = rows(&projection, "NameCount");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["name"], "Grace",
        "the first event is below the window"
    );
    assert!(
        projection.to_string().contains("the window starts at position 2"),
        "{projection}"
    );
}

// --- what it does not do ---------------------------------------------------

/// The property the whole design turns on: it reads through a follower, so it runs
/// against a directory a server is writing to. The analogue of `tests/plan.rs`'s
/// `replay_runs_while_a_server_holds_the_directory`.
#[test]
fn a_projection_runs_while_a_server_holds_the_directory() {
    let data = tempfile::tempdir().unwrap();
    let harness = boot_example_at(data.path());
    register_user(&harness.rt, ALICE, "alice@example.com", "Ada");
    register_user(&harness.rt, BOB, "bob@example.com", "Ada");

    let dir = support::example_dir("users");
    let projection = project(&dir, data.path(), BY_NAME);
    assert_eq!(projection.events, 2, "a live writer costs the fold nothing");

    harness.shutdown();
}

/// Nothing is deployed and nothing is recorded: not a segment byte, not a read model,
/// not a declaration row.
#[test]
fn a_projection_writes_nothing_to_the_data_directory() {
    let data = tempfile::tempdir().unwrap();
    let harness = boot_example_at(data.path());
    register_user(&harness.rt, ALICE, "alice@example.com", "Ada");
    harness.shutdown();

    let events = data.path().join("events");
    let before = support::tree(&events);
    assert!(!before.is_empty(), "the fixture wrote a segment");
    let projectors_before = support::tree(&data.path().join("projectors"));

    let dir = support::example_dir("users");
    let projection = project(&dir, data.path(), BY_NAME);
    assert_eq!(projection.events, 1, "the fold actually ran");

    assert_eq!(before, support::tree(&events), "an event segment changed");
    assert_eq!(
        projectors_before,
        support::tree(&data.path().join("projectors")),
        "a read model appeared for a projector that was never deployed"
    );
}

// --- compiling the scratch module ------------------------------------------

/// The one rule that is relaxed for a scratch module. A one-off question must not
/// require editing the project to ask it.
#[test]
fn a_scratch_projector_outside_projectors_is_not_a_placement_error() {
    let dir = support::example_dir("users");
    let project = load(&dir, BY_NAME);
    assert!(!project.has_errors(), "{:?}", project.findings);
    assert_eq!(project.scratch.as_deref(), Some(SCRATCH));
    assert!(
        project
            .projectors
            .iter()
            .any(|unit| unit.rel_path == SCRATCH && unit.def.name() == "ByName")
    );
}

/// The relaxation is narrow. A command cannot be folded over anything, so the existing
/// placement rule is the right answer and stays the answer.
#[test]
fn a_command_in_a_scratch_file_is_still_a_placement_error() {
    let dir = support::example_dir("users");
    let project = load(
        &dir,
        "\
command DoThing(user_id: Uuid) {
  emit @user.welcomed { user_id }
}
",
    );
    let errors = support::errors(&project);
    assert!(
        errors
            .iter()
            .any(|err| err.contains("must be declared under commands/")),
        "{errors:?}"
    );
}

/// The scratch file is the *second* module heklang sees, so a name it shares with a
/// deployed declaration is reported against the file the operator just wrote.
#[test]
fn a_name_that_collides_with_a_deployed_projector_points_at_the_scratch_file() {
    let dir = support::example_dir("users");
    let project = load(
        &dir,
        "\
projector Users {
  entity Thing { id: String @key @max(20) }
  on @user.registered { name } { put Thing { id: name } }
}
",
    );
    let collision = project
        .findings
        .iter()
        .find(|finding| finding.message.contains("Users"))
        .unwrap_or_else(|| panic!("no collision finding in {:?}", project.findings));
    assert_eq!(collision.location, SCRATCH, "{collision:?}");
}

/// A projector with no handler lowers to a query that matches nothing, so its empty
/// rows would read exactly like a question with no answer.
#[test]
fn a_projector_with_no_handler_is_refused() {
    let dir = support::example_dir("users");
    let data = tempfile::tempdir().unwrap();
    let project = load(
        &dir,
        "projector Silent { entity Thing { id: String @key @max(20) } }",
    );
    let Err(err) = projection::run(
        &project,
        data.path(),
        &Request::default(),
        &mut |_, _, _| {},
    ) else {
        panic!("the projection should have been refused")
    };
    assert!(
        format!("{err:#}").contains("declares no handler"),
        "{err:#}"
    );
}

#[test]
fn two_projectors_need_one_named() {
    let dir = support::example_dir("users");
    let source = format!(
        "{BY_NAME}\nprojector Other {{\n  entity T {{ name: String @key @max(120) }}\n  on @user.registered {{ name }} {{ put T {{ name }} }}\n}}\n"
    );
    let project = load(&dir, &source);

    let Err(err) = projection::select(&project, None) else {
        panic!("two projectors and no name should be ambiguous")
    };
    let text = format!("{err:#}");
    assert!(text.contains("--projector"), "{text}");
    assert!(text.contains("ByName, Other"), "{text}");

    let chosen = projection::select(&project, Some("Other")).expect("named");
    assert_eq!(chosen.def.name(), "Other");
}

/// Naming a deployed projector is the likely mistake, so the refusal says where its rows
/// already are instead of only saying no.
#[test]
fn naming_a_deployed_projector_says_where_its_rows_already_are() {
    let dir = support::example_dir("users");
    let project = load(&dir, BY_NAME);
    let Err(err) = projection::select(&project, Some("Users")) else {
        panic!("a deployed projector is not a scratch one")
    };
    let text = format!("{err:#}");
    assert!(text.contains("is deployed"), "{text}");
    assert!(text.contains("/read/Users/"), "{text}");
}

/// A scratch project is a question, so nothing may boot one: a runtime over it would
/// record the declaration and build the read model the whole command exists to avoid.
#[test]
fn a_scratch_project_cannot_be_served() {
    let data = tempfile::tempdir().unwrap();
    let dir = support::example_dir("users");
    let project = load(&dir, BY_NAME);
    let Err(err) = hekla::runtime::Runtime::open_quiescent(&project, data.path(), None) else {
        panic!("a scratch project must not open a runtime")
    };
    assert!(format!("{err:#}").contains("scratch module"), "{err:#}");
}

// --- sealed columns and erasure --------------------------------------------

/// The argument for the real sink, as one assertion. `last_email` is a different column
/// name than the event's `email`, and a key store binds the field name into the
/// ciphertext, so a sink that carried the log's bytes through would store something this
/// column cannot read back.
#[test]
fn a_sealed_column_is_re_sealed_and_reads_back_as_plaintext() {
    let orders = orders_project();
    let data = tempfile::tempdir().unwrap();
    let harness = Boot::new(orders.path())
        .data_dir(data.path())
        .with_master_key()
        .start();
    place_order(&harness.rt, support::UUID_A, 1, "ada@example.test");
    place_order(&harness.rt, support::UUID_B, 2, "grace@example.test");
    harness.shutdown();

    let projection = project_with(orders.path(), data.path(), BY_CUSTOMER, sealed_request());
    let rows = rows(&projection, "PerCustomer");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["last_email"], "ada@example.test");
    assert_eq!(rows[1]["last_email"], "grace@example.test");
    assert_eq!(projection.shredded.writes, 0);
}

#[test]
fn no_decrypt_prints_the_stored_ciphertext() {
    let orders = orders_project();
    let data = tempfile::tempdir().unwrap();
    let harness = Boot::new(orders.path())
        .data_dir(data.path())
        .with_master_key()
        .start();
    place_order(&harness.rt, support::UUID_A, 1, "ada@example.test");
    harness.shutdown();

    let projection = project_with(
        orders.path(),
        data.path(),
        BY_CUSTOMER,
        Request {
            decrypt: false,
            ..sealed_request()
        },
    );
    let rows = rows(&projection, "PerCustomer");
    assert_ne!(rows[0]["last_email"], "ada@example.test");
    assert!(rows[0]["last_email"].is_string(), "still a stored value");
}

/// An erased subject's column reads absent, and the count comes from the sink rather
/// than from an inference over the rows.
#[test]
fn an_erased_subject_reads_absent_and_is_counted() {
    let orders = orders_project();
    let data = tempfile::tempdir().unwrap();
    let harness = Boot::new(orders.path())
        .data_dir(data.path())
        .with_master_key()
        .start();
    place_order(&harness.rt, support::UUID_A, 1, "ada@example.test");
    place_order(&harness.rt, support::UUID_B, 2, "grace@example.test");
    // Through the running runtime's own key store, so the shred is the one an operator's
    // `hekla erase` performs and not a second implementation of it.
    let erased = harness
        .rt
        .keystore()
        .expect("a key store")
        .erase("customer_id", "2")
        .unwrap();
    assert!(erased);
    harness.shutdown();

    let projection = project_with(orders.path(), data.path(), BY_CUSTOMER, sealed_request());
    let rows = rows(&projection, "PerCustomer");
    assert_eq!(
        rows.len(),
        2,
        "the row survives; only the column is shredded"
    );
    assert_eq!(rows[0]["last_email"], "ada@example.test");
    assert!(rows[1].get("last_email").is_none(), "{:?}", rows[1]);
    assert_eq!(projection.shredded.writes, 1);
    assert_eq!(projection.shredded.subjects(), 1);
    assert!(
        projection.to_string().contains("1 erased subject(s)"),
        "{projection}"
    );
}

/// The control, and the case an inference over the rows would get wrong: a column the
/// handler never wrote is absent for a reason that has nothing to do with erasure.
#[test]
fn an_absent_optional_is_not_counted_as_an_erasure() {
    let orders = orders_project();
    let data = tempfile::tempdir().unwrap();
    let harness = Boot::new(orders.path())
        .data_dir(data.path())
        .with_master_key()
        .start();
    harness
        .rt
        .execute(
            "PlaceOrder",
            serde_json::json!({ "order_id": support::UUID_A, "customer_id": 1, "email": null }),
            &support::ctx(),
            None,
        )
        .unwrap();
    harness.shutdown();

    let projection = project_with(orders.path(), data.path(), BY_CUSTOMER, sealed_request());
    let rows = rows(&projection, "PerCustomer");
    assert_eq!(rows.len(), 1);
    assert!(rows[0].get("last_email").is_none(), "{:?}", rows[0]);
    assert_eq!(
        projection.shredded.writes, 0,
        "an optional nobody wrote is not an erasure"
    );
    assert!(
        !projection.to_string().contains("erased subject"),
        "{projection}"
    );
}

/// The fold re-seals, so it needs the key. Refused before the scan rather than a million
/// events into one.
#[test]
fn a_sealed_projection_is_refused_without_a_master_key() {
    let orders = orders_project();
    let data = tempfile::tempdir().unwrap();
    let harness = Boot::new(orders.path())
        .data_dir(data.path())
        .with_master_key()
        .start();
    place_order(&harness.rt, support::UUID_A, 1, "ada@example.test");
    harness.shutdown();

    // No master, which is the whole case: the key is an input rather than an ambient
    // fact, so its absence is testable here instead of only in a subprocess.
    let project = load(orders.path(), BY_CUSTOMER);
    let Err(err) = projection::run(
        &project,
        data.path(),
        &Request::default(),
        &mut |_, _, _| {},
    ) else {
        panic!("the projection should have been refused")
    };
    let text = format!("{err:#}");
    assert!(text.contains("HEKLA_MASTER_KEY"), "{text}");
    assert!(text.contains("last_email"), "{text}");
}

// --- the CLI surface -------------------------------------------------------

fn run_cli(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_hekla"))
        .args(args)
        .output()
        .expect("hekla runs")
}

#[test]
fn the_cli_puts_only_json_on_stdout() {
    let data = tempfile::tempdir().unwrap();
    let harness = boot_example_at(data.path());
    register_user(&harness.rt, ALICE, "alice@example.com", "Ada");
    harness.shutdown();

    let scratch = tempfile::tempdir().unwrap();
    let file = scratch.path().join("question.hk");
    fs::write(&file, BY_NAME).unwrap();

    let out = run_cli(&[
        "project",
        file.to_str().unwrap(),
        support::example_dir("users").to_str().unwrap(),
        "--data-dir",
        data.path().to_str().unwrap(),
        "--json",
    ]);
    assert!(out.status.success(), "{out:?}");
    let parsed: Value = serde_json::from_slice(&out.stdout).expect("stdout parses as JSON");
    assert_eq!(parsed["projector"], "ByName");
    assert_eq!(parsed["scanned"]["events"], 1);
}

/// `[DIR]` defaults to `.`, which makes the transposed form easy to type. The complaint
/// has to name the real mistake rather than the directory.
#[test]
fn the_cli_refuses_a_file_that_is_not_a_projection() {
    let out = run_cli(&["project", "."]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty(), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("is not a file"), "{err}");
}

#[test]
fn the_cli_refuses_a_directory_that_is_not_a_project() {
    let scratch = tempfile::tempdir().unwrap();
    let file = scratch.path().join("question.hk");
    fs::write(&file, BY_NAME).unwrap();

    let out = run_cli(&[
        "project",
        file.to_str().unwrap(),
        scratch.path().join("nope").to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("is not a directory"), "{err}");
}

/// A file the project's own walk already read must not be compiled a second time.
/// `hekla project question.hk` from the project root is the most natural invocation
/// there is.
#[test]
fn a_scratch_file_inside_the_project_is_compiled_once() {
    let data = tempfile::tempdir().unwrap();
    let project_dir = support::write_project(&[
        ("events/user.hk", USER_EVENTS),
        ("commands/register.hk", REGISTER),
    ]);
    let harness = Boot::new(project_dir.path()).data_dir(data.path()).start();
    harness
        .rt
        .execute(
            "Register",
            serde_json::json!({ "user_id": ALICE, "name": "Ada" }),
            &support::ctx(),
            None,
        )
        .unwrap();
    harness.shutdown();

    let inside = project_dir.path().join("question.hk");
    fs::write(&inside, BY_NAME).unwrap();
    let out = run_cli(&[
        "project",
        inside.to_str().unwrap(),
        project_dir.path().to_str().unwrap(),
        "--data-dir",
        data.path().to_str().unwrap(),
    ]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(!err.contains("declared twice"), "{err}");
}

const USER_EVENTS: &str = "\
event @user.registered {
  user_id: Uuid,
  name: String @max(120),
}
";

const REGISTER: &str = "\
command Register(user_id: Uuid, name: String) {
  emit @user.registered { user_id, name }
}
";
