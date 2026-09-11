//! A deployment must be able to read its own log.
//!
//! A log outlives the program that wrote it, so a declaration that has moved on from
//! what is stored breaks every reader of that type at once: a fold answers 500, a
//! projector rebuild fails and retries forever, and an effect lane wedges pinning the
//! watermark. hekla reads the oldest stored event of each declared type at boot and
//! refuses to start rather than serve any of that.
//!
//! Every test here is two deploys. The first writes history; the second rewrites the
//! declaration and boots against the same data directory. Following `tests/plan.rs`'s
//! rule: a checker that never fires is indistinguishable from one that works, so the
//! edits that must boot are tested beside the ones that must not.

use std::fs;
use std::path::Path;

use hekla::projector::Readiness;
use hekla::runtime::Runtime;
use serde_json::{Value, json};
use tempfile::TempDir;
use uuid::Uuid;

mod support;

use support::{Boot, Harness, ctx, log_head, read_row, wait_until};

// --- fixtures --------------------------------------------------------------

const EVENTS: &str = r#"
event @order.placed {
  order_id: Uuid,
  total: Int,
  // FIELD
}
"#;

const PLACE_ORDER: &str = r#"
command PlaceOrder(order_id: Uuid, total: Int) {
  emit @order.placed { order_id, total }
}
"#;

const ORDERS: &str = r#"
projector Orders {
  entity Order {
    order_id: Uuid @key,
    total: Int,
    // COLUMN
  }

  on @order.placed { order_id, total } {
    put Order { order_id, total }
  }
}
"#;

/// A command whose fold binds the younger field, so the read that used to answer 500
/// is exercised rather than assumed. It emits nothing: the status is the answer.
const TOUCH_ORDER: &str = r#"
refusal Unseen "the fold read no note"

command TouchOrder(order_id: Uuid) {
  fold seen: String = ""
    on @order.placed(order_id) { note } => note

  if seen == "" {
    return reject Unseen
  }
}
"#;

/// Write the project as one deploy sees it.
///
/// `field` is what the event gains, and an empty one is the first deploy. A field an
/// author adds is emitted by the command and carried by the read model, because that is
/// what adding a field actually looks like; `column` is its entity type, which differs
/// from the event's when the field is optional.
fn write(dir: &Path, field: &str, column: &str) {
    let added = !field.is_empty();
    let events = EVENTS.replace("// FIELD", field);
    let (command, projector) = if added {
        (
            PLACE_ORDER.replace("order_id, total }", r#"order_id, total, note: "fresh" }"#),
            ORDERS
                .replace("// COLUMN", column)
                .replace("order_id, total }", "order_id, total, note }"),
        )
    } else {
        (PLACE_ORDER.to_owned(), ORDERS.to_owned())
    };

    let mut files = vec![
        ("events/order.hk", events),
        ("commands/place-order.hk", command),
        ("projectors/orders.hk", projector),
    ];
    // The fold binds the field under its own name, so it can only be written where the
    // field is a `String`. An optional one is a different read and has its own test.
    if added && !field.contains('?') {
        files.push(("commands/touch-order.hk", TOUCH_ORDER.to_owned()));
    }
    write_files(dir, &files);
}

/// Write these files and remove every other `.hk` the last deploy left, so a redeploy
/// cannot inherit a module that names a field this declaration no longer has.
fn write_files(dir: &Path, files: &[(&str, String)]) {
    let optional = "commands/touch-order.hk";
    if !files.iter().any(|(name, _)| *name == optional) {
        drop(fs::remove_file(dir.join(optional)));
    }
    put(dir, files);
}

fn put(dir: &Path, files: &[(&str, String)]) {
    for (rel, content) in files {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

/// A project of exactly these two modules, deployed once with one order in the log.
///
/// The shared fixture above varies one field of one event, which is the common case.
/// These vary the shape of the declaration itself, so they build what they need.
fn seeded(event: &str, command: &str) -> (TempDir, TempDir) {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    put(
        project.path(),
        &[
            ("events/order.hk", event.to_owned()),
            ("commands/place-order.hk", command.to_owned()),
        ],
    );
    let first = boot(project.path(), data.path());
    let placed = first
        .rt
        .execute(
            "PlaceOrder",
            json!({ "order_id": Uuid::new_v4().to_string() }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(placed.status, 200, "{:?}", placed.body);
    first.shutdown();
    (project, data)
}

/// Deploy the first declaration and leave one order in the log.
fn with_history() -> (TempDir, TempDir, String) {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write(project.path(), "", "");
    let first = boot(project.path(), data.path());
    let order_id = Uuid::new_v4().to_string();
    place(&first.rt, &order_id);
    first.shutdown();
    (project, data, order_id)
}

fn boot(project_dir: &Path, data_dir: &Path) -> Harness {
    Boot::new(project_dir).data_dir(data_dir).start()
}

fn refusal(project_dir: &Path, data_dir: &Path) -> String {
    let err = Boot::new(project_dir)
        .data_dir(data_dir)
        .try_start()
        .err()
        .expect("this program cannot read the log it is being deployed over");
    format!("{err:#}")
}

fn place(rt: &Runtime, order_id: &str) {
    let result = rt
        .execute(
            "PlaceOrder",
            json!({ "order_id": order_id, "total": 7 }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 200, "{:?}", result.body);
}

/// One projected order, after the rebuild this redeploy triggers has swapped its model
/// in. Waiting on the position alone would read the *old* model, which is already past
/// the head and has no column for the new field.
fn order_row(harness: &Harness, order_id: &str) -> Value {
    let shared = harness.rt.projector("Orders").unwrap();
    wait_until("the projector to rebuild under the new declaration", || {
        shared.readiness() == Readiness::Ready && shared.replays_completed() >= 1
    });
    let head = log_head(&harness.rt);
    read_row(harness, "Orders", "Order", order_id, head).expect("the order was projected")
}

// --- a field the stored payload cannot answer ------------------------------

/// The case the whole feature exists for. Nothing in heklang can catch it, because
/// nothing in heklang knows there is a log.
#[test]
fn a_field_added_to_an_event_with_history_refuses_the_boot() {
    let (project, data, _) = with_history();
    write(project.path(), "note: String,", "note: String,");

    let message = refusal(project.path(), data.path());
    assert!(message.contains("`@order.placed`"), "{message}");
    assert!(
        message.contains("no stored event can carry `note`"),
        "{message}"
    );
    assert!(message.contains("@absent"), "{message}");
    assert!(
        message.contains("nothing has been recorded"),
        "and it says the repair is free: {message}"
    );
}

/// The fix the refusal names, and the point of the annotation: history reads as the
/// literal, through both the reader that answered 500 and the one that stalled.
#[test]
fn an_absent_value_lets_the_same_deploy_boot_and_read_the_literal() {
    let (project, data, order_id) = with_history();
    write(
        project.path(),
        r#"note: String @absent("none given"),"#,
        "note: String,",
    );

    let second = boot(project.path(), data.path());
    assert_eq!(
        order_row(&second, &order_id)["note"],
        json!("none given"),
        "the projector rebuild replays the old event and reads the annotation"
    );

    let folded = second
        .rt
        .execute("TouchOrder", json!({ "order_id": order_id }), &ctx(), None)
        .unwrap();
    assert_eq!(
        folded.status, 200,
        "a fold binding the younger field used to answer 500: {:?}",
        folded.body
    );
    second.shutdown();
}

/// The other fix, and the one that was already there. `?` says absence is part of the
/// domain; `@absent` says the field is younger than the log. Both answer the question.
#[test]
fn an_optional_field_added_to_an_event_with_history_boots() {
    let (project, data, order_id) = with_history();
    write(project.path(), "note: String?,", "note: String?,");

    let second = boot(project.path(), data.path());
    let row = order_row(&second, &order_id);
    assert!(
        row.get("note").is_none(),
        "an absent column is omitted rather than null: {row}"
    );
    second.shutdown();
}

/// The refusal is about history, not about the declaration. A type nothing has written
/// has nothing to answer for, so the same edit deploys.
#[test]
fn a_field_added_to_an_event_with_no_history_boots() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write(project.path(), "", "");
    boot(project.path(), data.path()).shutdown();

    write(project.path(), "note: String,", "note: String,");
    boot(project.path(), data.path()).shutdown();
}

// --- the controls: edits that must still boot ------------------------------

/// A field the declaration no longer lists is no longer decoded, which is what makes
/// "declare the new shape under a new name" a real repair rather than advice.
#[test]
fn a_field_removed_from_an_event_with_history_boots() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    write(project.path(), "note: String,", "note: String,");
    let first = boot(project.path(), data.path());
    place(&first.rt, &Uuid::new_v4().to_string());
    first.shutdown();

    write(project.path(), "", "");
    boot(project.path(), data.path()).shutdown();
}

/// Redeploying the same program must not read as a change. A probe that fired here
/// would refuse every restart of a healthy deployment.
#[test]
fn redeploying_the_same_declaration_boots() {
    let (project, data, _) = with_history();
    boot(project.path(), data.path()).shutdown();
}

// --- a record one level down -----------------------------------------------

const RECORD_EVENTS: &str = r#"
record Note {
  kind: String,
  // FIELD
}

event @order.placed {
  order_id: Uuid,
  detail: Note,
}
"#;

const RECORD_COMMAND: &str = r#"
command PlaceOrder(order_id: Uuid) {
  emit @order.placed { order_id, detail: Note { kind: "gift" } }
}
"#;

/// A record reached from an event carries that event's history, so the question is the
/// same one level down and the answer has to say which level.
#[test]
fn a_record_field_added_below_an_event_with_history_refuses_and_names_the_path() {
    let (project, data) = seeded(RECORD_EVENTS, RECORD_COMMAND);
    put(
        project.path(),
        &[
            (
                "events/order.hk",
                RECORD_EVENTS.replace("// FIELD", "weight: Int,"),
            ),
            (
                "commands/place-order.hk",
                RECORD_COMMAND.replace(r#"kind: "gift" }"#, r#"kind: "gift", weight: 1 }"#),
            ),
        ],
    );

    let message = refusal(project.path(), data.path());
    assert!(
        message.contains("no stored event can carry `detail.weight`"),
        "the path says which field of which record: {message}"
    );
    assert!(message.contains("@absent"), "{message}");
}

/// The same edit with the annotation the refusal names.
#[test]
fn a_record_field_that_answers_absence_boots() {
    let (project, data) = seeded(RECORD_EVENTS, RECORD_COMMAND);
    put(
        project.path(),
        &[
            (
                "events/order.hk",
                RECORD_EVENTS.replace("// FIELD", "weight: Int @absent(0),"),
            ),
            (
                "commands/place-order.hk",
                RECORD_COMMAND.replace(r#"kind: "gift" }"#, r#"kind: "gift", weight: 1 }"#),
            ),
        ],
    );
    boot(project.path(), data.path()).shutdown();
}

// --- a stored value that no longer fits ------------------------------------

const TYPED_EVENTS: &str = r#"
event @order.placed {
  order_id: Uuid,
  total: Int,
}
"#;

const TYPED_COMMAND: &str = r#"
command PlaceOrder(order_id: Uuid) {
  emit @order.placed { order_id, total: 7 }
}
"#;

/// The other way a declaration outruns its log, and the reason the refusal is one rule
/// rather than two. `@absent` cannot answer this, so the message must not offer it as
/// though it could.
#[test]
fn a_changed_field_type_refuses_and_does_not_offer_an_annotation_that_cannot_help() {
    let (project, data) = seeded(TYPED_EVENTS, TYPED_COMMAND);
    put(
        project.path(),
        &[
            (
                "events/order.hk",
                TYPED_EVENTS.replace("total: Int,", "total: String,"),
            ),
            (
                "commands/place-order.hk",
                TYPED_COMMAND.replace("total: 7", r#"total: "7""#),
            ),
        ],
    );

    let message = refusal(project.path(), data.path());
    assert!(
        message.contains("total: expected String, stored a number"),
        "it names the field and what was there: {message}"
    );
    assert!(
        message.contains("under a new name"),
        "and the repair, which is a new field rather than an annotation: {message}"
    );
    assert!(
        !message.contains("say what it reads as with `@absent"),
        "the absence guidance must not appear for a field that is present: {message}"
    );
}

// --- an enum, which is neither of those ------------------------------------

const ENUM_EVENTS: &str = r#"
enum Channel { Email, Sms }

event @order.placed {
  order_id: Uuid,
  channel: Channel,
}
"#;

const ENUM_COMMAND: &str = r#"
command PlaceOrder(order_id: Uuid) {
  emit @order.placed { order_id, channel: Email }
}
"#;

/// The probe is empirical, not a diff: it asks whether this program can read that
/// event, so a change that stored values still fit is not a change it has an opinion
/// about. Without this the check would refuse every widened enum.
#[test]
fn a_widened_enum_boots() {
    let (project, data) = seeded(ENUM_EVENTS, ENUM_COMMAND);
    put(
        project.path(),
        &[
            (
                "events/order.hk",
                ENUM_EVENTS.replace("{ Email, Sms }", "{ Email, Sms, Post }"),
            ),
            ("commands/place-order.hk", ENUM_COMMAND.to_owned()),
        ],
    );
    boot(project.path(), data.path()).shutdown();
}

/// And narrowing one is caught, which nothing about field names could have told us.
#[test]
fn an_enum_that_loses_a_stored_variant_refuses() {
    let (project, data) = seeded(ENUM_EVENTS, ENUM_COMMAND);
    put(
        project.path(),
        &[
            (
                "events/order.hk",
                ENUM_EVENTS.replace("{ Email, Sms }", "{ Sms }"),
            ),
            (
                "commands/place-order.hk",
                ENUM_COMMAND.replace("Email", "Sms"),
            ),
        ],
    );

    let message = refusal(project.path(), data.path());
    assert!(
        message.contains("channel: expected Channel, stored a variant it does not have"),
        "{message}"
    );
}

// --- the refusal leaves nothing behind -------------------------------------

/// Rule 16: a boot that refuses must leave no trace of having happened. Recording the
/// candidate and then bailing would make the next `hekla plan` compare the candidate
/// against itself and report that nothing would change, for a deploy that never ran.
#[test]
fn a_refused_boot_records_no_declaration() {
    let (project, data, _) = with_history();
    write(project.path(), "note: String,", "note: String,");
    refusal(project.path(), data.path());

    // Put the deployed declaration back. If the refused boot had recorded itself, this
    // would read as a change; what is on disk is what is running.
    write(project.path(), "", "");
    let plan = hekla::plan::compute_with(
        &support::load_ok(project.path()),
        data.path(),
        hekla::plan::Replay::Off,
    )
    .expect("a plan against the directory the first deploy wrote");
    assert!(
        plan.changes.is_empty(),
        "the refused deploy recorded itself: {plan}"
    );
}

// --- what `plan` says before the deploy ------------------------------------
//
// These live here rather than in `tests/plan.rs` because they turn on this rule's
// fixture rather than on plan's own machinery, and duplicating the fixture to put them
// beside the other plan tests would give the rule two places to drift.

fn plan_of(project_dir: &Path, data_dir: &Path) -> hekla::plan::Plan {
    hekla::plan::compute_with(
        &support::load_ok(project_dir),
        data_dir,
        hekla::plan::Replay::Off,
    )
    .expect("a plan against a deployed directory")
}

/// The deploy gate. A `contract` change on an event says the shape moved; it cannot say
/// the deployment stops being able to read itself, which is the part that matters.
#[test]
fn plan_names_history_this_deploy_could_not_read() {
    let (project, data, _) = with_history();
    write(project.path(), "note: String,", "note: String,");

    let plan = plan_of(project.path(), data.path());
    let found = plan.unreadable.as_deref().expect("the log was asked");
    assert_eq!(found.len(), 1, "{plan}");
    assert_eq!(found[0].event_type, "order.placed", "{plan}");
    assert!(found[0].is_absence(), "{plan}");
    assert!(
        !plan.is_empty(),
        "a deploy that would not boot is not nothing"
    );

    let rendered = format!("{plan}");
    assert!(
        rendered.contains("serving would refuse to start"),
        "and it says so in the same words the credential check does: {rendered}"
    );
    assert!(
        rendered.contains("@absent"),
        "and names the fix before the deploy rather than after it: {rendered}"
    );
}

/// The fix, seen from the gate: asked, and clean. Distinct from not asked, which is
/// what a caller reading `--json` has to be able to tell apart.
#[test]
fn plan_reports_an_answered_field_as_asked_and_clean() {
    let (project, data, _) = with_history();
    write(
        project.path(),
        r#"note: String @absent("none given"),"#,
        "note: String,",
    );

    let plan = plan_of(project.path(), data.path());
    assert!(
        plan.unreadable.as_deref().is_some_and(<[_]>::is_empty),
        "the log was read and found readable: {plan}"
    );
    assert_eq!(plan.json()["unreadable"], json!([]), "{plan}");
}

/// `plan` opens no event log unless it has a reason to, which is the property the rest
/// of the command rests on. A deploy that moves no declaration capable of decoding an
/// event has no reason.
#[test]
fn plan_leaves_the_log_unopened_when_no_event_shape_moved() {
    let (project, data, _) = with_history();
    write_files(
        project.path(),
        &[(
            "commands/noop.hk",
            "command Noop(order_id: Uuid) {\n  let _seen = order_id\n}\n".to_owned(),
        )],
    );

    let plan = plan_of(project.path(), data.path());
    assert!(
        plan.changes.iter().any(|change| change.name == "Noop"),
        "the deploy really does change something: {plan}"
    );
    assert!(
        plan.unreadable.is_none(),
        "not asked, which is not the same answer as asked and clean: {plan}"
    );
    assert_eq!(plan.json()["unreadable"], json!(null), "{plan}");
}

// --- a boundary keyed on a field the log predates --------------------------

/// A warning rather than an error, in the "a judgement about a design" category: the
/// slice is well formed and hekla has no business refusing it. It is worth saying
/// because the annotation makes the field look answerable everywhere, and a tag index
/// is the one place it is not.
#[test]
fn a_boundary_filtering_on_an_absent_field_warns() {
    let project = tempfile::tempdir().unwrap();
    put(
        project.path(),
        &[
            (
                "events/order.hk",
                EVENTS.replace("// FIELD", r#"note: String @absent("none given"),"#),
            ),
            (
                "commands/place-order.hk",
                r#"
command PlaceOrder(order_id: Uuid, total: Int, note: String) {
  fold seen: Bool = false
    on @order.placed(order_id, note) => true

  if !seen {
    emit @order.placed { order_id, total, note }
  }
}
"#
                .to_owned(),
            ),
        ],
    );

    let warnings = support::findings(&support::load_ok(project.path()));
    assert!(
        warnings.iter().any(|finding| finding.message.contains(
            "which `@absent` marks as younger than the log; an event written before that existed"
        )),
        "{warnings:?}"
    );
}

/// And the control. A boundary on a field that has always been there carries no such
/// caveat, so the warning has to stay quiet for it.
#[test]
fn a_boundary_filtering_on_an_ordinary_field_does_not_warn() {
    let project = tempfile::tempdir().unwrap();
    put(
        project.path(),
        &[
            ("events/order.hk", EVENTS.replace("// FIELD", "")),
            (
                "commands/place-order.hk",
                r#"
command PlaceOrder(order_id: Uuid, total: Int) {
  fold seen: Bool = false
    on @order.placed(order_id) => true

  if !seen {
    emit @order.placed { order_id, total }
  }
}
"#
                .to_owned(),
            ),
        ],
    );

    let warnings = support::findings(&support::load_ok(project.path()));
    assert!(
        !warnings
            .iter()
            .any(|finding| finding.message.contains("@absent")),
        "{warnings:?}"
    );
}

// --- verify, which replays everything -------------------------------------

/// `verify` replays every projector and every effect against the log, so a program that
/// cannot read it reports a rebuild failure and a divergence per invocation: corruption
/// findings for a directory that has none. It refuses for the same reason `serve` does,
/// which is the third time `open_quiescent` repeats a guard `open` applies.
#[test]
fn verify_refuses_a_directory_this_program_cannot_read() {
    let (project, data, _) = with_history();
    write(project.path(), "note: String,", "note: String,");

    let err = hekla::verify::sweep(&support::load_ok(project.path()), data.path(), None)
        .expect_err("a sweep of a log this program cannot read");
    let message = format!("{err:#}");
    assert!(message.contains("it cannot verify it"), "{message}");
    assert!(
        message.contains("no stored event can carry `note`"),
        "{message}"
    );
    assert!(
        message.contains("the sweep can be re-run"),
        "and the repair is the one a sweep has, not the one a deploy has: {message}"
    );
}

/// And the control: the same directory sweeps clean once the declaration answers for
/// it. Deployed first, because a sweep compares the live read model against a rebuilt
/// one and the model on disk is still the shape the previous deploy left.
#[test]
fn verify_sweeps_a_directory_whose_fields_answer_absence() {
    let (project, data, _) = with_history();
    write(
        project.path(),
        r#"note: String @absent("none given"),"#,
        "note: String,",
    );
    let second = boot(project.path(), data.path());
    support::quiesce(&second);
    second.shutdown();

    let report = hekla::verify::sweep(&support::load_ok(project.path()), data.path(), None)
        .expect("the sweep should run");
    assert!(report.is_clean(), "{report}");
}

// --- what each half of the check reaches -----------------------------------

/// **The documented limit of the sampled half, pinned so it cannot drift silently.**
///
/// Narrowing an enum breaks only the events that stored a variant it lost, so whether
/// the probe sees it depends on which event it samples. The declaration table cannot
/// settle it either: no field was added, so the complete half has nothing to say. A
/// complete answer here means decoding every event in the log at every boot, which is
/// the cost this design declines to pay.
///
/// If this ever starts refusing, the limit has been closed and ARCHITECTURE.md section 4
/// and ROADMAP.md phase 33 both need correcting.
#[test]
fn a_narrowed_enum_is_missed_when_the_oldest_event_kept_a_surviving_variant() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    put(
        project.path(),
        &[
            ("events/order.hk", ENUM_EVENTS.to_owned()),
            (
                "commands/place-order.hk",
                r#"
command PlaceOrder(order_id: Uuid, sms: Bool) {
  if sms {
    emit @order.placed { order_id, channel: Sms }
  }
  if !sms {
    emit @order.placed { order_id, channel: Email }
  }
}
"#
                .to_owned(),
            ),
        ],
    );
    let first = boot(project.path(), data.path());
    for sms in [true, false] {
        let placed = first
            .rt
            .execute(
                "PlaceOrder",
                json!({ "order_id": Uuid::new_v4().to_string(), "sms": sms }),
                &ctx(),
                None,
            )
            .unwrap();
        assert_eq!(placed.status, 200, "{:?}", placed.body);
    }
    first.shutdown();

    // Position 1 is `Sms` and survives the narrowing; position 2 is `Email` and does not.
    put(
        project.path(),
        &[
            (
                "events/order.hk",
                ENUM_EVENTS.replace("{ Email, Sms }", "{ Sms }"),
            ),
            (
                "commands/place-order.hk",
                r#"
command PlaceOrder(order_id: Uuid, sms: Bool) {
  emit @order.placed { order_id, channel: Sms }
}
"#
                .to_owned(),
            ),
        ],
    );
    // Position 1 holds `Sms` and still decodes, so the sample says nothing; position 2
    // holds `Email` and does not. `an_enum_that_loses_a_stored_variant_refuses` is the
    // same edit over a log whose oldest event did use the lost variant, and that one is
    // caught: the difference is which event the probe happened to read.
    Boot::new(project.path())
        .data_dir(data.path())
        .try_start()
        .expect("the sampled half reads position 1 only, which still decodes")
        .shutdown();
}

/// **What the complete half is for.** Add a field, remove it, add it back: the oldest
/// event holds it and so does the newest, and every event in between holds neither. No
/// single sampled event can see this, and the declaration table settles it without
/// reading one, because it keeps every version rather than only the current.
#[test]
fn a_field_removed_and_added_back_refuses_for_the_events_in_between() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    // v1: the field exists.
    write(project.path(), "note: String,", "note: String,");
    let v1 = boot(project.path(), data.path());
    place_note(&v1.rt, &Uuid::new_v4().to_string());
    v1.shutdown();

    // v2: the field is gone, and an event is written without it.
    write(project.path(), "", "");
    let v2 = boot(project.path(), data.path());
    place(&v2.rt, &Uuid::new_v4().to_string());
    v2.shutdown();

    // v3: the field comes back, with nothing to answer for the v2-era event.
    write(project.path(), "note: String,", "note: String,");
    let message = refusal(project.path(), data.path());
    assert!(
        message.contains("no stored event can carry `note`"),
        "the v2-era event carries neither, whatever the two ends hold: {message}"
    );
}

fn place_note(rt: &Runtime, order_id: &str) {
    let result = rt
        .execute(
            "PlaceOrder",
            json!({ "order_id": order_id, "total": 7 }),
            &ctx(),
            None,
        )
        .unwrap();
    assert_eq!(result.status, 200, "{:?}", result.body);
}
