//! `hekla backup`: copying a data directory a server is still writing, and what a restore
//! owes it.
//!
//! What the command has to keep true:
//!
//! - **The log may be copied from under the writer.** tephra mutates nothing and deletes
//!   nothing, and a batch counts only if every record in it validates by CRC and the run
//!   ends in a commit marker, so a file-level copy is a committed prefix however badly it
//!   is timed.
//! - **The op-DB is one `VACUUM INTO` away from a consistent snapshot**, taken through a
//!   read-only connection while the server commits into it.
//! - **The order is forced, and then it is not enough.** The key store must be at least as
//!   new as the log, because a missing subject key is indistinguishable from an erasure.
//!   The effect tables must be no newer, because a restored log continues at `head + 1`
//!   and `begin_invocation` keys on `(effect, position)` alone. Both live in `hekla.db`,
//!   so the second constraint is repaired after the copy rather than ordered for.
//!
//! The erasure cases are the reason the ordering matters at all. An erase deletes a subject
//! key and leaves no ledger, so the question a backup raises is whether a restore resurrects
//! what an erase destroyed. It does not, for two separate reasons, and both are pinned below.
//!
//! The tests that take the halves in the wrong order on purpose call
//! [`copy_log`](hekla::backup::copy_log), [`snapshot_state`](hekla::backup::snapshot_state)
//! and [`clamp_to_log`](hekla::backup::clamp_to_log) directly. Everything else goes through
//! `backup::run`, and the refusals go through the binary.
//!
//! One section builds its log through tephra rather than through a runtime, with segments small
//! enough to roll over. `Runtime` uses 256 MiB a segment, so a real rollover is otherwise out
//! of a test's reach, and rollover is where the reuse rule was wrong: a follower takes each
//! segment's size from the file, so hekla's own read paths work over a log built that way.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use hekla::backup;
use hekla::effect::StubHttpClient;
use hekla::lock::DataDirLock;
use hekla::opdb::OpDb;
use hekla::progress::Progress;
use hekla::runtime::Runtime;
use serde_json::{Value, json};
use tempfile::TempDir;
use tephra::{
    Event, EventType, Position, Query, SegmentConfig, SegmentSet, Tag, Tags, WriteCoordinator,
    WriterConfig,
};

mod support;

use support::{Boot, Harness, UUID_A, UUID_B, UUID_C, ctx, write_project};

const EFFECT: &str = "Forget";

const EVENTS: &str = r#"
subject Customer(Int)

event @customer.closed {
  closure_id: Uuid,
  customer_id: Customer,
  // Optional because an erased subject's column reads back absent, which is what every
  // assertion here turns on.
  email: String? @subject(customer_id) @max(200),
}
"#;

const COMMANDS: &str = r#"
command CloseAccount(closure_id: Uuid, customer_id: Customer, email: String?) {
  emit @customer.closed { closure_id, customer_id, email }
}
"#;

const PROJECTOR: &str = r#"
projector Closures {
  entity Closure {
    closure_id: Uuid @key,
    customer_id: Customer @index,
    email: String? @max(200),
  }

  on @customer.closed { closure_id, customer_id, email } {
    put Closure { closure_id, customer_id, email }
  }
}
"#;

const FORGET: &str = r#"
effect Forget {
  on @customer.closed { @key customer_id, email } {
    http.post("https://example.test/farewell", { "to": reveal(email) })
    erase(customer_id)
  }
}
"#;

/// The project without the erasing effect: a deployment where a closed account's email is
/// still readable.
fn without_effect() -> TempDir {
    write_project(&[
        ("events/customer.hk", EVENTS),
        ("commands/close-account.hk", COMMANDS),
        ("projectors/closures.hk", PROJECTOR),
    ])
}

fn with_effect() -> TempDir {
    write_project(&[
        ("events/customer.hk", EVENTS),
        ("commands/close-account.hk", COMMANDS),
        ("projectors/closures.hk", PROJECTOR),
        ("effects/forget.hk", FORGET),
    ])
}

fn boot(project: &Path, data: &Path, http: Arc<StubHttpClient>) -> Harness {
    Boot::new(project)
        .data_dir(data)
        .http(http)
        .with_master_key()
        .start()
}

/// Close an account and return the log position of the appended event.
fn close(rt: &Runtime, closure_id: &str, customer: u64, email: &str) -> u64 {
    let body = json!({
        "closure_id": closure_id,
        "customer_id": customer,
        "email": email,
    });
    let result = rt.execute("CloseAccount", body, &ctx(), None).unwrap();
    assert_eq!(result.status, 200, "CloseAccount failed: {:?}", result.body);
    result.body["positions"]["last"].as_u64().unwrap()
}

// --- driving the backup ----------------------------------------------------

fn run(data: &Path, target: &Path) -> backup::Report {
    backup::run(data, target, &Progress::new(false)).expect("the backup should run")
}

/// The CLI, which is where the refusals are worth asserting: an operator sees the message and
/// the exit code, not a `Result`.
fn cli(data: &Path, target: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hekla"))
        .arg("backup")
        .arg(data)
        .arg(target)
        .arg("--no-progress")
        .output()
        .unwrap()
}

/// `hekla verify` against a data directory, with the master key the harness uses, since a
/// project that declares a subject is refused without one.
fn verify_cli(project: &Path, data: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hekla"))
        .arg("verify")
        .arg(project)
        .arg("--data-dir")
        .arg(data)
        .env("HEKLA_MASTER_KEY", STANDARD.encode(support::MASTER_KEY))
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn refused(output: &Output, needle: &str) {
    assert!(!output.status.success(), "should have refused: {output:?}");
    assert!(
        stderr(output).contains(needle),
        "expected `{needle}`, got {}",
        stderr(output)
    );
}

fn manifest(target: &Path) -> Value {
    let text = fs::read_to_string(target.join(backup::MANIFEST)).unwrap();
    serde_json::from_str(&text).unwrap()
}

// --- reading what a restore holds -----------------------------------------

fn subject_key_present(data: &Path, customer: u64) -> bool {
    OpDb::open(&data.join("hekla.db"))
        .unwrap()
        .get_subject_key("Customer", &customer.to_string())
        .unwrap()
        .is_some()
}

/// The sealed email as the read API hands it out, which is where a key that is gone shows up
/// as `null`.
fn email(harness: &Harness, closure_id: &str, after: u64) -> Value {
    support::read_row(harness, "Closures", "Closure", closure_id, after)
        .expect("a row for the closure")["email"]
        .clone()
}

// --- the copy is safe under a live writer ---------------------------------

/// The claim the whole feature rests on: a copy taken with no coordination at all, while
/// commands are landing and the effect is working through them, restores to a directory that
/// boots and passes the invariant sweep.
#[test]
fn a_backup_of_a_directory_under_load_restores_and_verifies() {
    let project = with_effect();
    let data = tempfile::tempdir().unwrap();
    let live = boot(project.path(), data.path(), Arc::new(StubHttpClient::ok()));

    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let rt = Arc::clone(&live.rt);
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let mut customer = 100;
            while !stop.load(Ordering::Relaxed) {
                close(
                    &rt,
                    &uuid::Uuid::new_v4().to_string(),
                    customer,
                    "load@example.test",
                );
                customer += 1;
                thread::sleep(Duration::from_millis(2));
            }
            customer - 100
        })
    };

    let target = tempfile::tempdir().unwrap();
    let report = run(data.path(), target.path());
    stop.store(true, Ordering::Relaxed);
    let appended = writer.join().unwrap();
    assert!(appended > 0, "the writer should have been busy throughout");
    assert!(
        report.log_head > 0,
        "and the copy should hold some of what it wrote"
    );
    // The copy is a *strict* prefix of the log it was taken from. Appended here rather than
    // left to the writer thread: whether that thread gets a slice between the copy reading the
    // segment and `run` returning is the scheduler's business, and the claim is about the copy.
    close(&live.rt, UUID_A, 1, "after@example.test");
    let reached = live.rt.log_head();
    assert!(
        report.log_head < reached,
        "the copy ({}) should trail the log it was taken from ({reached})",
        report.log_head
    );
    live.shutdown();

    // No read models: a restore rebuilds them, and the summary says which.
    assert!(!target.path().join("projectors").exists());
    assert_eq!(report.projectors_skipped, ["Closures"]);
    // The lock is taken for the run and released with it, so the target is not left claimed.
    drop(DataDirLock::acquire(target.path()).expect("the target should be free"));

    let restored = boot(
        project.path(),
        target.path(),
        Arc::new(StubHttpClient::ok()),
    );
    assert_eq!(
        restored.rt.log_head(),
        report.log_head,
        "the restored log is the prefix the copy pinned, with no torn record above it"
    );
    support::quiesce(&restored);
    restored.shutdown();

    let sweep = support::sweep(project.path(), target.path());
    assert!(
        sweep.is_clean(),
        "the restored directory should verify: {:?}",
        sweep.violations
    );
}

/// The manifest carries the two facts a restore cannot work out for itself: which master key
/// the backup is inert without, and what it will have to rebuild.
#[test]
fn the_manifest_names_the_master_key_and_the_read_models() {
    // Without the erasing effect, which would shred the very key this is about.
    let project = without_effect();
    let data = tempfile::tempdir().unwrap();
    let live = boot(project.path(), data.path(), Arc::new(StubHttpClient::ok()));
    let position = close(&live.rt, UUID_A, 42, "gone@example.test");
    support::quiesce(&live);
    live.shutdown();

    let target = tempfile::tempdir().unwrap();
    let report = run(data.path(), target.path());
    let manifest = manifest(target.path());

    assert_eq!(manifest["log_head"], json!(position));
    assert_eq!(manifest["projectors_skipped"], json!(["Closures"]));
    assert_eq!(
        manifest["master_key_ids"].as_array().map(Vec::len),
        Some(1),
        "the key the backup is wrapped under: {manifest}"
    );
    assert_eq!(manifest["keys"], json!(1));
    assert_eq!(report.schema_version, hekla::opdb::SCHEMA_VERSION);
}

/// Through the CLI, which is what an operator gets: the summary, and a zero exit.
#[test]
fn the_command_reports_what_it_copied() {
    let project = without_effect();
    let data = tempfile::tempdir().unwrap();
    let live = boot(project.path(), data.path(), Arc::new(StubHttpClient::ok()));
    close(&live.rt, UUID_A, 42, "gone@example.test");
    support::quiesce(&live);
    live.shutdown();

    let target = tempfile::tempdir().unwrap();
    let output = cli(data.path(), target.path());
    assert!(output.status.success(), "{}", stderr(&output));
    let summary = stdout(&output);
    for needle in [
        "log head 1",
        "subject key(s), wrapped under master",
        "clamped to the log head",
        "a restore rebuilds 1 read model(s): Closures",
        "check it with `hekla verify",
    ] {
        assert!(summary.contains(needle), "expected `{needle}` in {summary}");
    }
}

// --- a log that has rolled over -------------------------------------------

/// Small enough that a few hundred kilobyte-sized events roll the log over.
///
/// Rollover is the one thing a backup cannot be shown correct without, and it is where the
/// reuse rule's first version was wrong: a segment copied while it was being appended to kept
/// its `fallocate`d length when it sealed, so matching on name and length alone reused the
/// truncated copy for ever. `Runtime` uses 256 MiB a segment, which puts a real rollover out
/// of a test's reach, so these build the log through tephra directly and then drive the whole
/// command over it. Nothing here interprets the events, so they need no envelope: what is
/// under test is that the copy is the same log.
const ROLLOVER_SEGMENT: usize = 64 * 1024;

/// Append `count` kilobyte events to the log under `data`, creating it if it is not there.
fn append_events(data: &Path, count: usize) {
    let set = SegmentSet::open(data.join("events"), SegmentConfig::new(ROLLOVER_SEGMENT)).unwrap();
    // A batch has to fit a segment, and the default allows 8 MiB. Irrelevant to what is being
    // tested, since these append one event at a time.
    let writer = WriterConfig {
        max_batch_bytes: ROLLOVER_SEGMENT / 2,
        ..WriterConfig::default()
    };
    let (coordinator, handle) = WriteCoordinator::start(set, writer).unwrap();
    let event_type = EventType::new("filler").unwrap();
    let tags = Tags::new([Tag::new("fill:1").unwrap()]).unwrap();
    for n in 0..count {
        let payload = format!("{{\"n\":{n},\"pad\":\"{}\"}}", "x".repeat(900));
        let event = Event::new(&event_type, &tags, payload.as_bytes()).unwrap();
        handle.append(vec![event], None).unwrap();
    }
    coordinator.shutdown();
}

/// A data directory the command will accept: a real log, and the operational database a fresh
/// deployment would have.
fn synthetic_deployment(data: &Path, events: usize) {
    append_events(data, events);
    OpDb::open(&data.join("hekla.db")).unwrap();
}

fn segments(data: &Path) -> usize {
    fs::read_dir(data.join("events"))
        .unwrap()
        .filter(|entry| {
            let name = entry.as_ref().unwrap().file_name();
            name.to_string_lossy().ends_with(".log")
        })
        .count()
}

/// Every event in a directory's log, by position and payload, read through hekla's own
/// follower. The strongest thing a backup can be asked to be: the same log.
fn logged(data: &Path) -> Vec<(u64, Vec<u8>)> {
    let store = hekla::runtime::follow(data)
        .unwrap()
        .expect("the directory holds a log");
    store
        .read(&Query::All, Position::new(0), None)
        .collect_owned()
        .unwrap()
        .into_iter()
        .map(|(position, event)| (position.get(), event.data().to_vec()))
        .collect()
}

/// A multi-segment log copies whole, through the real command, and reads back event for event.
#[test]
fn a_rolled_over_log_is_copied_whole_and_reads_back_identically() {
    let data = tempfile::tempdir().unwrap();
    synthetic_deployment(data.path(), 200);
    assert!(
        segments(data.path()) > 1,
        "the fixture has to have rolled over, or this tests nothing: {} segment(s)",
        segments(data.path())
    );

    let target = tempfile::tempdir().unwrap();
    let report = run(data.path(), target.path());

    assert_eq!(report.log_head, 200);
    assert_eq!(segments(target.path()), segments(data.path()));
    assert_eq!(
        logged(target.path()),
        logged(data.path()),
        "the copy is the same log"
    );
}

/// The regression test for the worst thing this command got wrong.
///
/// A segment that was still being appended to when it was copied is a short prefix of a file
/// whose length never changes. Once it seals, a reuse check that trusted the length kept that
/// prefix, and the backup was permanently missing every event appended after the first run.
#[test]
fn a_segment_still_growing_when_it_was_copied_is_whole_once_it_seals() {
    let data = tempfile::tempdir().unwrap();
    synthetic_deployment(data.path(), 20);
    assert_eq!(segments(data.path()), 1, "one segment, still being written");

    let target = tempfile::tempdir().unwrap();
    let first = run(data.path(), target.path());
    assert_eq!(first.log_head, 20);

    // The deployment carries on until the segment the copy holds a prefix of has sealed.
    append_events(data.path(), 200);
    assert!(
        segments(data.path()) > 1,
        "it has to seal, or this tests nothing"
    );

    let second = run(data.path(), target.path());
    assert_eq!(second.log_head, 220);
    assert!(
        second.files_copied >= 2,
        "the sealed segment and the new ones: {second}"
    );
    assert_eq!(
        logged(target.path()),
        logged(data.path()),
        "a sealed segment that grew since it was copied has to be copied again; reusing it on \
         its length left the log short of everything after the first run"
    );
}

// --- erasure survives a restore -------------------------------------------

/// The first reason a restore does not resurrect an erasure: the state snapshot is taken
/// after the log, so an erase that has completed is already absent from it.
#[test]
fn a_state_snapshot_taken_after_the_erase_carries_it() {
    let project = with_effect();
    let data = tempfile::tempdir().unwrap();
    let live = boot(project.path(), data.path(), Arc::new(StubHttpClient::ok()));

    let position = close(&live.rt, UUID_A, 42, "gone@example.test");
    support::wait_effect_position(&live.rt, EFFECT, position);
    assert!(
        !subject_key_present(data.path(), 42),
        "the effect erased the customer"
    );

    let target = tempfile::tempdir().unwrap();
    run(data.path(), target.path());
    live.shutdown();
    assert!(
        !subject_key_present(target.path(), 42),
        "and the snapshot has no key to put back"
    );

    let again = Arc::new(StubHttpClient::ok());
    let restored = boot(project.path(), target.path(), again.clone());
    support::quiesce(&restored);
    assert_eq!(
        email(&restored, UUID_A, position),
        Value::Null,
        "the restored copy reads as erased"
    );
    assert_eq!(
        again.call_count(),
        0,
        "and the journal came with it, so nothing was re-sent"
    );
    restored.shutdown();
}

/// The second reason, and the one that needs no discipline from the operator: an erase the
/// backup predates re-applies itself. `erase` is journaled, so a replay that finds no entry
/// performs it for real.
///
/// The effect is deployed only on the restore, which is how a backup gets a key store that
/// still holds the key and a journal with no record of the erase. A deployment whose effect
/// had simply not reached the event yet leaves the same two facts on disk.
#[test]
fn a_backup_taken_before_the_erase_re_applies_it_on_restore() {
    let bare = without_effect();
    let data = tempfile::tempdir().unwrap();
    let live = boot(bare.path(), data.path(), Arc::new(StubHttpClient::ok()));

    let position = close(&live.rt, UUID_A, 42, "gone@example.test");
    assert_eq!(
        email(&live, UUID_A, position),
        json!("gone@example.test"),
        "nothing has erased 42 in this deployment"
    );

    let target = tempfile::tempdir().unwrap();
    run(data.path(), target.path());
    live.shutdown();
    assert!(
        subject_key_present(target.path(), 42),
        "so the backup holds a readable key"
    );

    let full = with_effect();
    let stub = Arc::new(StubHttpClient::ok());
    let restored = boot(full.path(), target.path(), stub.clone());
    support::wait_effect_position(&restored.rt, EFFECT, position);
    assert_eq!(
        email(&restored, UUID_A, position),
        Value::Null,
        "the journal miss made the replay perform the erase"
    );
    assert_eq!(
        stub.call_count(),
        1,
        "along with the farewell it never sent"
    );
    restored.shutdown();
    assert!(!subject_key_present(target.path(), 42));
}

/// The failure the command exists to prevent, and the reason the order is not a matter of
/// taste. A key store copied *before* the log it is paired with is missing the keys for
/// everything appended in between, and a missing key is indistinguishable from an erasure: the
/// restore reads as though those customers had been forgotten, with nothing erased and nothing
/// reported.
#[test]
fn a_state_snapshot_older_than_the_log_reads_as_erased() {
    let bare = without_effect();
    let data = tempfile::tempdir().unwrap();
    let live = boot(bare.path(), data.path(), Arc::new(StubHttpClient::ok()));
    let target = tempfile::tempdir().unwrap();
    let state = target.path().join("hekla.db");

    // The wrong way round: the state first, and the key for 42 is minted by the append that
    // follows it.
    backup::snapshot_state(data.path(), &state).unwrap();
    let position = close(&live.rt, UUID_A, 42, "present@example.test");
    backup::copy_log(data.path(), target.path(), &Progress::new(false)).unwrap();
    let head = backup::log_head(target.path()).unwrap();
    backup::clamp_to_log(&state, head).unwrap();

    assert_eq!(
        email(&live, UUID_A, position),
        json!("present@example.test"),
        "readable where it was written"
    );
    live.shutdown();

    let restored = boot(bare.path(), target.path(), Arc::new(StubHttpClient::ok()));
    assert_eq!(
        email(&restored, UUID_A, position),
        Value::Null,
        "and silently gone in the copy"
    );
    restored.shutdown();
}

// --- the clamp ------------------------------------------------------------

/// A target whose log stops at the first closure and whose state has the second recorded as
/// terminal. Every live backup has this shape to some degree: the log is copied first, so the
/// state is always a little ahead of it.
fn a_target_with_an_ahead_journal(project: &Path, data: &Path, target: &Path) {
    let live = boot(project, data, Arc::new(StubHttpClient::ok()));

    let first = close(&live.rt, UUID_A, 1, "one@example.test");
    support::wait_effect_position(&live.rt, EFFECT, first);
    backup::copy_log(data, target, &Progress::new(false)).unwrap();

    let second = close(&live.rt, UUID_B, 2, "two@example.test");
    support::wait_effect_position(&live.rt, EFFECT, second);
    backup::snapshot_state(data, &target.join("hekla.db")).unwrap();
    live.shutdown();

    assert_eq!((first, second), (1, 2), "the fixture's positions");
}

/// Without the clamp the restore is quietly wrong, and in the direction that loses work: a new
/// event lands on a position the discarded half of the state already calls terminal, so
/// `begin_invocation` reports `AlreadyTerminal` for an event it has never seen.
#[test]
fn without_the_clamp_a_reused_position_is_silently_skipped() {
    let project = with_effect();
    let data = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    a_target_with_an_ahead_journal(project.path(), data.path(), target.path());

    let stub = Arc::new(StubHttpClient::ok());
    let restored = boot(project.path(), target.path(), stub.clone());
    let position = close(&restored.rt, UUID_C, 3, "three@example.test");
    assert_eq!(position, 2, "the restored log reuses the position");
    support::quiesce(&restored);

    assert_eq!(stub.call_count(), 0, "the effect never ran");
    assert_eq!(
        restored.rt.effect(EFFECT).unwrap().last_error(),
        None,
        "and said nothing about it"
    );
    restored.shutdown();
    assert!(
        subject_key_present(target.path(), 3),
        "so customer 3 was never erased"
    );
}

/// With it, the same restore runs the position, and the clamp says what it had to discard to
/// get there.
#[test]
fn the_clamp_lets_a_reused_position_run() {
    let project = with_effect();
    let data = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    a_target_with_an_ahead_journal(project.path(), data.path(), target.path());

    assert_eq!(
        backup::log_head(target.path()).unwrap(),
        1,
        "the copied log stops at 1"
    );
    let discarded = backup::clamp_to_log(&target.path().join("hekla.db"), 1).unwrap();
    assert_eq!(discarded.invocations, 1, "the record of position 2");
    assert_eq!(discarded.cursors_lowered, 1);

    let stub = Arc::new(StubHttpClient::ok());
    let restored = boot(project.path(), target.path(), stub.clone());
    let position = close(&restored.rt, UUID_C, 3, "three@example.test");
    assert_eq!(position, 2);
    support::wait_effect_position(&restored.rt, EFFECT, position);

    assert_eq!(stub.call_count(), 1, "the effect ran for the new event");
    restored.shutdown();
    assert!(
        !subject_key_present(target.path(), 3),
        "and erased customer 3"
    );
}

// --- refusals -------------------------------------------------------------

/// A project directory names its deployment as well as its data directory does, which is the
/// spelling an operator standing in a checkout has to hand. `<dir>/data` is the whole of the
/// convention, since `hekla.toml` cannot move it.
#[test]
fn a_project_directory_names_the_deployment_it_holds() {
    let project = without_effect();
    let data = project.path().join("data");
    let live = boot(project.path(), &data, Arc::new(StubHttpClient::ok()));
    let position = close(&live.rt, UUID_A, 42, "gone@example.test");
    support::quiesce(&live);
    live.shutdown();

    let target = tempfile::tempdir().unwrap();
    let output = cli(project.path(), target.path());
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(manifest(target.path())["log_head"], json!(position));
    assert_eq!(
        manifest(target.path())["source"],
        json!(fs::canonicalize(&data).unwrap()),
        "and the manifest records the data directory, not the project"
    );
    // The same deployment by its other name, into the same target: one directory, so the
    // identity check has to see through the two spellings rather than refuse.
    let again = cli(&data, target.path());
    assert!(again.status.success(), "{}", stderr(&again));
}

/// Neither spelling resolving is a mistyped path, and the message names both rather than
/// guessing which was meant.
#[test]
fn it_refuses_a_source_that_is_neither_a_data_directory_nor_a_project_holding_one() {
    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let output = cli(source.path(), target.path());
    refused(&output, "no data directory to back up: neither");
    assert!(
        stderr(&output).contains("data"),
        "the message names both places it looked: {}",
        stderr(&output)
    );
}

/// Every guard a target gets reads its manifest, so a manifest that cannot be read is a
/// refusal rather than an absence: carrying on would mean copying with the identity check off,
/// and that check is what stands between one target and two deployments' segments interleaved
/// into a single log whose records all pass their CRC.
///
/// hekla cannot have produced one, because the write goes through a rename.
#[test]
fn it_refuses_a_target_whose_manifest_cannot_be_read() {
    let project = without_effect();
    let data = tempfile::tempdir().unwrap();
    let live = boot(project.path(), data.path(), Arc::new(StubHttpClient::ok()));
    close(&live.rt, UUID_A, 1, "one@example.test");
    support::quiesce(&live);

    let target = tempfile::tempdir().unwrap();
    run(data.path(), target.path());
    fs::write(target.path().join(backup::MANIFEST), b"{\"log_head\": 1").unwrap();

    let output = cli(data.path(), target.path());
    live.shutdown();
    refused(&output, "corrupted from outside");
    assert!(
        stderr(&output).contains("take a fresh backup"),
        "and says what to do: {}",
        stderr(&output)
    );
}

/// Serving from a target is how a restore works, and a target that has been appended to has a
/// log of its own. Backing up over it would overwrite the segments the two share by name and
/// leave the rest, splicing two timelines into one chain.
#[test]
fn it_refuses_a_target_whose_own_log_has_moved_ahead() {
    let project = without_effect();
    let data = tempfile::tempdir().unwrap();
    let live = boot(project.path(), data.path(), Arc::new(StubHttpClient::ok()));
    close(&live.rt, UUID_A, 1, "one@example.test");
    support::quiesce(&live);
    live.shutdown();

    let target = tempfile::tempdir().unwrap();
    let report = run(data.path(), target.path());

    // Restored by being served from, and then used.
    let restored = boot(
        project.path(),
        target.path(),
        Arc::new(StubHttpClient::ok()),
    );
    let moved = close(&restored.rt, UUID_B, 2, "two@example.test");
    support::quiesce(&restored);
    restored.shutdown();
    assert!(moved > report.log_head);

    refused(
        &cli(data.path(), target.path()),
        "which is ahead of the 1 at",
    );
}

/// A log only grows, so a file in the target that the source does not have came from somewhere
/// else. Leaving it beside the segments a run copies would hand tephra a chain built from two
/// logs.
#[test]
fn a_run_removes_a_target_file_the_source_does_not_have() {
    let project = without_effect();
    let data = tempfile::tempdir().unwrap();
    let live = boot(project.path(), data.path(), Arc::new(StubHttpClient::ok()));
    close(&live.rt, UUID_A, 1, "one@example.test");
    support::quiesce(&live);

    let target = tempfile::tempdir().unwrap();
    run(data.path(), target.path());
    let stray = target.path().join("events/00000000000000009999.log");
    fs::write(&stray, b"from another log").unwrap();

    run(data.path(), target.path());
    live.shutdown();
    assert!(!stray.exists(), "a backup is a mirror, not an accumulation");
}

/// Read models are not part of a backup, and a target that has been booted has built its own.
/// Keeping them would hand a restore models built from an older definition, which
/// `auto_rebuild = false` reports as stale rather than rebuilding.
#[test]
fn a_run_clears_read_models_a_boot_of_the_target_left() {
    let project = without_effect();
    let data = tempfile::tempdir().unwrap();
    let live = boot(project.path(), data.path(), Arc::new(StubHttpClient::ok()));
    close(&live.rt, UUID_A, 1, "one@example.test");
    support::quiesce(&live);

    let target = tempfile::tempdir().unwrap();
    run(data.path(), target.path());
    let booted = boot(
        project.path(),
        target.path(),
        Arc::new(StubHttpClient::ok()),
    );
    support::quiesce(&booted);
    booted.shutdown();
    assert!(
        target.path().join("projectors/Closures.db").exists(),
        "the boot should have built one"
    );

    let report = run(data.path(), target.path());
    live.shutdown();
    assert!(
        !target.path().join("projectors").exists(),
        "and the next run takes it back out"
    );
    assert_eq!(
        report.projectors_skipped,
        ["Closures"],
        "so the summary's promise that a restore rebuilds them is true again"
    );
}

/// The refusal that matters most: a target that holds something other than a backup might be
/// somebody's data directory, and this command would otherwise write a log over it.
#[test]
fn it_refuses_to_write_into_a_directory_that_is_not_a_backup() {
    let project = without_effect();
    let data = tempfile::tempdir().unwrap();
    boot(project.path(), data.path(), Arc::new(StubHttpClient::ok())).shutdown();

    let target = tempfile::tempdir().unwrap();
    fs::write(target.path().join("something-else"), b"mine").unwrap();
    refused(
        &cli(data.path(), target.path()),
        "is not empty and holds no hekla-backup.json",
    );
}

/// A build that does not know every table a position can be recorded in cannot clamp one, and
/// a clamp that misses a table leaves exactly the silent skip it exists to prevent.
#[test]
fn it_refuses_a_source_from_a_newer_build() {
    let project = without_effect();
    let data = tempfile::tempdir().unwrap();
    boot(project.path(), data.path(), Arc::new(StubHttpClient::ok())).shutdown();
    rusqlite::Connection::open(data.path().join("hekla.db"))
        .unwrap()
        .pragma_update(None, "user_version", 99i64)
        .unwrap();

    let target = tempfile::tempdir().unwrap();
    refused(
        &cli(data.path(), target.path()),
        "is at schema version 99 and this build knows up to",
    );
}

/// An append-only log cannot fall behind its own backup. The path can be the same one and the
/// directory still be a different directory, which is what a wiped and re-created deployment
/// is, and backing that up over its predecessor's target would interleave the two.
#[test]
fn it_refuses_a_target_that_is_ahead_of_the_source() {
    let project = without_effect();
    let data = tempfile::tempdir().unwrap();
    let live = boot(project.path(), data.path(), Arc::new(StubHttpClient::ok()));
    close(&live.rt, UUID_A, 42, "gone@example.test");
    support::quiesce(&live);
    live.shutdown();

    let target = tempfile::tempdir().unwrap();
    run(data.path(), target.path());

    // Wiped and started again at the same path, so the identity check passes and only the
    // head says anything is wrong.
    for entry in fs::read_dir(data.path()).unwrap() {
        let path = entry.unwrap().path();
        let _ = if path.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
    }
    boot(project.path(), data.path(), Arc::new(StubHttpClient::ok())).shutdown();
    refused(
        &cli(data.path(), target.path()),
        "which is ahead of the 0 at",
    );
}

/// A target belongs to one data directory. Interleaving two sources' segments into one log
/// would produce records that all pass their CRC and a position chain that may well be
/// contiguous, which is the worst way for this to go wrong.
#[test]
fn it_refuses_a_target_that_holds_a_backup_of_another_directory() {
    let project = without_effect();
    let first = tempfile::tempdir().unwrap();
    let live = boot(project.path(), first.path(), Arc::new(StubHttpClient::ok()));
    close(&live.rt, UUID_A, 1, "one@example.test");
    close(&live.rt, UUID_B, 2, "two@example.test");
    support::quiesce(&live);
    live.shutdown();

    let target = tempfile::tempdir().unwrap();
    run(first.path(), target.path());

    // Another directory, far enough along that the ahead-of-the-source guard does not fire.
    let second = tempfile::tempdir().unwrap();
    let other = boot(
        project.path(),
        second.path(),
        Arc::new(StubHttpClient::ok()),
    );
    for customer in 1..=4 {
        close(
            &other.rt,
            &uuid::Uuid::new_v4().to_string(),
            customer,
            "other@example.test",
        );
    }
    support::quiesce(&other);
    other.shutdown();

    refused(&cli(second.path(), target.path()), "not of");
}

/// The window the `complete` flag exists for. A target whose log has been extended past the key
/// store beside it is the silent-erasure shape itself, so it must not read as a backup.
#[test]
fn an_interrupted_run_leaves_the_target_marked_incomplete_and_the_next_run_repairs_it() {
    let project = without_effect();
    let data = tempfile::tempdir().unwrap();
    let live = boot(project.path(), data.path(), Arc::new(StubHttpClient::ok()));
    let first = close(&live.rt, UUID_A, 1, "one@example.test");
    support::quiesce(&live);

    let target = tempfile::tempdir().unwrap();
    run(data.path(), target.path());
    assert_eq!(manifest(target.path())["complete"], json!(true));

    // What a run killed between the log copy and the state landing leaves behind: the claim,
    // and a log ahead of the key store beside it.
    let second = close(&live.rt, UUID_B, 2, "two@example.test");
    support::quiesce(&live);
    backup::copy_log(data.path(), target.path(), &Progress::new(false)).unwrap();
    let claim = serde_json::json!({
        "complete": false,
        "source": fs::canonicalize(data.path()).unwrap(),
        "log_head": first,
    });
    fs::write(
        target.path().join(backup::MANIFEST),
        serde_json::to_string(&claim).unwrap(),
    )
    .unwrap();
    assert_eq!(backup::log_head(target.path()).unwrap(), second);

    // A sweep over it *passes*: an absent key is a legitimate state, not a violation. So the
    // one thing that can catch this is the flag, and the command that is told to check a
    // backup is the one that has to read it.
    let swept = verify_cli(project.path(), target.path());
    assert!(swept.status.success(), "{}", stderr(&swept));
    assert!(
        stderr(&swept).contains("is a backup an interrupted run left"),
        "verify should say what it is looking at: {}",
        stderr(&swept)
    );

    // The next run writes over it rather than refusing, and the result is complete again.
    let report = run(data.path(), target.path());
    live.shutdown();
    assert_eq!(report.log_head, second);
    assert_eq!(manifest(target.path())["complete"], json!(true));
    assert!(subject_key_present(target.path(), 2), "and holds both keys");
    let swept = verify_cli(project.path(), target.path());
    assert!(
        !stderr(&swept).contains("interrupted"),
        "and the warning goes with the repair: {}",
        stderr(&swept)
    );
}

/// A run that dies before the state lands leaves a full-size staging copy of the operational
/// database at the target's root, which only a sweep outside `events/` can reach.
#[test]
fn a_staged_state_snapshot_left_by_a_dead_run_is_swept() {
    let project = without_effect();
    let data = tempfile::tempdir().unwrap();
    let live = boot(project.path(), data.path(), Arc::new(StubHttpClient::ok()));
    close(&live.rt, UUID_A, 1, "one@example.test");
    support::quiesce(&live);

    let target = tempfile::tempdir().unwrap();
    run(data.path(), target.path());
    let staged = target.path().join("hekla.db.incoming");
    fs::write(&staged, b"what a dead run left").unwrap();

    run(data.path(), target.path());
    live.shutdown();
    assert!(!staged.exists(), "the leftover should not outlive a run");
}

/// Serving from a mirror is a legitimate way to restore one, and a backup must not write into
/// it while that is happening.
#[test]
fn it_refuses_a_target_something_is_serving_from() {
    let project = without_effect();
    let data = tempfile::tempdir().unwrap();
    let live = boot(project.path(), data.path(), Arc::new(StubHttpClient::ok()));
    close(&live.rt, UUID_A, 42, "gone@example.test");
    support::quiesce(&live);
    live.shutdown();

    let target = tempfile::tempdir().unwrap();
    run(data.path(), target.path());

    let serving = boot(
        project.path(),
        target.path(),
        Arc::new(StubHttpClient::ok()),
    );
    refused(&cli(data.path(), target.path()), "which is in use");
    serving.shutdown();
}
