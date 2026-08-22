//! `kiln test`: events in, assert events out.
//!
//! A test file under `tests/` declares a module-level `cases` list of `case(...)`
//! scenarios. Each scenario runs the real command against a throwaway store seeded
//! with the `given` events, so `query` filtering and the command's own
//! `AppendCondition` are genuinely exercised, then the outcome is asserted against
//! `expect`. Seeding goes through the same `build_event` path as a live append, so
//! a seeded event is byte-identical to one the runtime would write.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use allocative::Allocative;
use anyhow::Context;
use starlark::any::ProvidesStaticType;
use starlark::environment::{FrozenModule, Globals, GlobalsBuilder};
use starlark::values::list::{ListRef, UnpackList};
use starlark::values::{NoSerialize, StarlarkValue, Value, ValueLike, starlark_value};
use starlark::{starlark_module, starlark_simple_value};
use tephra::{SegmentConfig, SegmentSet, WriteCoordinator, WriterConfig};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::context::CommandContext;
use crate::crypto::{KeyStore, MasterKeys};
use crate::dispatch::{self, CommandOutcome, EventDefs, build_event};
use crate::loader::{CommandUnit, LoadedProject};
use crate::opdb::OpDb;
use crate::starlark_builtins::{
    ConstructedEvent, EmitOutcome, EmittedEvent, InvalidInput, Rejection, runtime_builtins,
};
use std::fmt;

/// Throwaway per-case stores stay small, but the segment must still clear the
/// writer's default max batch size.
const TEST_SEGMENT_SIZE: usize = 16 * 1024 * 1024;
/// A fixed clock so a `now()`-using command is reproducible under test.
const TEST_NOW: &str = "1970-01-01T00:00:00Z";
/// A fixed, non-secret master key so `kiln test` can exercise subject-scoped
/// encryption deterministically without any environment setup. Test assertions
/// compare plaintext events, so the ciphertext values never surface.
const TEST_MASTER_KEY: [u8; 32] = [0x2a; 32];

/// What a scenario expects the command to do.
#[derive(Debug, Clone, Allocative)]
enum Expectation {
    /// These events, in order (compared by type, data and tags).
    Emit(Vec<ConstructedEvent>),
    /// A state-dependent rejection with this code (the message is not asserted).
    Reject { code: String },
    /// A malformed-input rejection (the message is not asserted).
    InvalidInput,
}

/// One `case(...)` scenario.
#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
pub struct TestCase {
    name: Option<String>,
    command: String,
    given: Vec<ConstructedEvent>,
    /// The command input as its JSON wire form.
    input: String,
    expect: Expectation,
}

impl fmt::Display for TestCase {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "case({})", self.label())
    }
}

impl TestCase {
    fn label(&self) -> String {
        match &self.name {
            Some(name) => name.clone(),
            None => self.command.clone(),
        }
    }
}

#[starlark_value(type = "test_case")]
impl<'v> StarlarkValue<'v> for TestCase {}
starlark_simple_value!(TestCase);

/// The `case(...)` builtin exposed to test files.
#[starlark_module]
pub fn test_builtins(builder: &mut GlobalsBuilder) {
    /// Declare a scenario. `given` are the prior events (constructed from event
    /// definitions), `input` is the command input dict, and `expect` is an
    /// `emit([...])`, `reject(...)` or `invalid_input(...)`.
    fn case<'v>(
        #[starlark(require = named)] command: String,
        #[starlark(require = named)] input: Value<'v>,
        #[starlark(require = named)] expect: Value<'v>,
        #[starlark(require = named)] given: Option<UnpackList<Value<'v>>>,
        #[starlark(require = named)] name: Option<String>,
    ) -> anyhow::Result<TestCase> {
        let given = match given {
            Some(list) => {
                let mut out = Vec::with_capacity(list.items.len());
                for item in &list.items {
                    let event = item.downcast_ref::<ConstructedEvent>().ok_or_else(|| {
                        anyhow::anyhow!(
                            "given items must be events from an event definition, got {}",
                            item.get_type()
                        )
                    })?;
                    out.push(event.clone());
                }
                out
            }
            None => Vec::new(),
        };
        let input_json = input
            .to_json_value()
            .map_err(|err| anyhow::anyhow!("input must be JSON-serialisable: {err}"))?;
        if !input_json.is_object() {
            anyhow::bail!("input must be a dict, got {}", input.get_type());
        }
        Ok(TestCase {
            name,
            command,
            given,
            input: input_json.to_string(),
            expect: parse_expectation(expect)?,
        })
    }
}

fn parse_expectation(value: Value<'_>) -> anyhow::Result<Expectation> {
    if let Some(emit) = value.downcast_ref::<EmitOutcome>() {
        return Ok(Expectation::Emit(emit.events.clone()));
    }
    if let Some(reject) = value.downcast_ref::<Rejection>() {
        return Ok(Expectation::Reject {
            code: reject.code.clone(),
        });
    }
    if value.downcast_ref::<InvalidInput>().is_some() {
        return Ok(Expectation::InvalidInput);
    }
    anyhow::bail!(
        "expect must be emit([...]), reject(...) or invalid_input(...), got {}",
        value.get_type()
    )
}

/// Globals for test files: the base builtins (so `emit`/`reject`/`invalid_input`
/// and event constructors work) plus `case(...)`.
fn test_globals() -> Globals {
    GlobalsBuilder::standard()
        .with(runtime_builtins)
        .with(test_builtins)
        .build()
}

/// Run `kiln test` over the project at `dir`.
pub fn run(dir: &Path) -> ExitCode {
    let project = LoadedProject::load(dir);
    // Reuse the CLI's collection so `kiln test` reports the same findings, in the
    // same location order, as `kiln check` and `kiln serve`.
    let findings = crate::cli::collect_findings(&project);
    let load_errors = findings
        .iter()
        .filter(|finding| finding.severity == crate::loader::Severity::Error)
        .count();
    if load_errors > 0 {
        for finding in &findings {
            if finding.severity == crate::loader::Severity::Error {
                eprintln!("error: {}: {}", finding.location, finding.message);
            }
        }
        eprintln!("cannot run tests: the project has {load_errors} error(s)");
        return ExitCode::FAILURE;
    }

    let globals = test_globals();
    let mut passed = 0usize;
    let mut failed = 0usize;

    for path in test_files(&dir.join("tests")) {
        let rel = rel_to_string(dir, &path);
        let src = match fs::read_to_string(&path) {
            Ok(src) => src,
            Err(err) => {
                eprintln!("error: {rel}: reading file: {err}");
                failed += 1;
                continue;
            }
        };
        let module = match project.eval_against_libraries(&rel, src, &globals) {
            Ok(module) => module,
            Err(err) => {
                eprintln!("error: {rel}: {err}");
                failed += 1;
                continue;
            }
        };
        let cases = match read_cases(&module) {
            Ok(cases) => cases,
            Err(err) => {
                eprintln!("error: {rel}: {err}");
                failed += 1;
                continue;
            }
        };
        for case in &cases {
            match run_case(&project, case) {
                Ok(()) => {
                    passed += 1;
                    println!("ok: {rel}: {}", case.label());
                }
                Err(detail) => {
                    failed += 1;
                    println!("FAIL: {rel}: {}: {detail}", case.label());
                }
            }
        }
    }

    println!("\n{passed} passed, {failed} failed");
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Read the `cases` list off a frozen test module.
fn read_cases(module: &FrozenModule) -> anyhow::Result<Vec<TestCase>> {
    let Some(owned) = module.get_option("cases")? else {
        anyhow::bail!("test file defines no `cases = [...]` list");
    };
    let list = ListRef::from_value(owned.value())
        .ok_or_else(|| anyhow::anyhow!("`cases` must be a list of case(...)"))?;
    let mut out = Vec::with_capacity(list.len());
    for item in list.iter() {
        let case = item.downcast_ref::<TestCase>().ok_or_else(|| {
            anyhow::anyhow!("`cases` items must be case(...), got {}", item.get_type())
        })?;
        out.push(case.clone());
    }
    Ok(out)
}

/// Run one scenario: seed a throwaway store with `given`, run the real command,
/// and assert the outcome. `Ok(())` is a pass; `Err(detail)` is a failure.
fn run_case(project: &LoadedProject, case: &TestCase) -> Result<(), String> {
    let command = project
        .commands
        .iter()
        .find(|unit| unit.loaded.def.name() == case.command)
        .ok_or_else(|| format!("no command named `{}`", case.command))?;

    match execute_case(command, &project.events.by_type, case) {
        Ok(outcome) => compare(&case.expect, &outcome),
        Err(err) => Err(format!("{err:#}")),
    }
}

fn execute_case(
    command: &CommandUnit,
    events: &EventDefs,
    case: &TestCase,
) -> anyhow::Result<CommandOutcome> {
    let dir = tempfile::tempdir().context("creating a temp store")?;
    let set = SegmentSet::open(
        dir.path().join("events"),
        SegmentConfig::new(TEST_SEGMENT_SIZE),
    )
    .context("opening the temp store")?;
    let (coordinator, store) = WriteCoordinator::start(set, WriterConfig::default())
        .context("starting the temp writer")?;

    // A per-case in-memory key store under the fixed test master key, so subject
    // fields encrypt and match just as they would live. Assertions compare plaintext
    // events, so the ciphertext never appears in a test.
    let opdb = Arc::new(Mutex::new(OpDb::open_in_memory()?));
    let keystore = KeyStore::new(opdb, MasterKeys::new(TEST_MASTER_KEY, vec![]));

    let seed_ctx = CommandContext::new(Uuid::new_v4());
    let mut packed = Vec::with_capacity(case.given.len());
    for event in &case.given {
        let data = serde_json::from_str(&event.data_json).unwrap_or(serde_json::Value::Null);
        let emitted = EmittedEvent {
            event_type: event.event_type.clone(),
            data,
            tags: event.tags.clone(),
        };
        packed.push(build_event(
            &emitted,
            events.get(&emitted.event_type),
            Some(&keystore),
            &seed_ctx,
            TEST_NOW,
            None,
        )?);
    }
    if !packed.is_empty() {
        store
            .append(packed, None)
            .map_err(|err| anyhow::anyhow!("seeding given events: {err}"))?;
    }

    let input: serde_json::Value =
        serde_json::from_str(&case.input).context("parsing the case input")?;
    let ctx = CommandContext::new(Uuid::new_v4());
    // Host-side validation is a first-class outcome, and the runtime performs it
    // once before dispatch; mirror that split here so tests exercise the same path.
    let outcome = match dispatch::validate_input(&command.loaded, &input) {
        Ok(()) => dispatch::run_command(
            &store,
            &command.loaded,
            events,
            Some(&keystore),
            &input,
            &ctx,
            TEST_NOW,
            None,
        )?,
        Err(err) => CommandOutcome::InvalidInput {
            message: format!("{err}"),
        },
    };
    coordinator.shutdown();
    Ok(outcome)
}

fn compare(expect: &Expectation, outcome: &CommandOutcome) -> Result<(), String> {
    match (expect, outcome) {
        (Expectation::Emit(expected), CommandOutcome::Committed { events, .. }) => {
            compare_events(expected, events)
        }
        (Expectation::Reject { code }, CommandOutcome::Rejected { code: actual, .. }) => {
            if actual == code {
                Ok(())
            } else {
                Err(format!("expected reject `{code}`, got reject `{actual}`"))
            }
        }
        (Expectation::InvalidInput, CommandOutcome::InvalidInput { .. }) => Ok(()),
        (expect, outcome) => Err(format!(
            "expected {}, got {}",
            describe_expect(expect),
            describe_outcome(outcome)
        )),
    }
}

fn compare_events(expected: &[ConstructedEvent], actual: &[EmittedEvent]) -> Result<(), String> {
    if expected.len() != actual.len() {
        return Err(format!(
            "expected {} event(s), got {}",
            expected.len(),
            actual.len()
        ));
    }
    for (idx, (exp, act)) in expected.iter().zip(actual).enumerate() {
        if exp.event_type != act.event_type {
            return Err(format!(
                "event {idx}: expected type `{}`, got `{}`",
                exp.event_type, act.event_type
            ));
        }
        let exp_data = serde_json::from_str(&exp.data_json).unwrap_or(serde_json::Value::Null);
        if exp_data != act.data {
            return Err(format!(
                "event {idx} (`{}`): expected data {exp_data}, got {}",
                exp.event_type, act.data
            ));
        }
        let mut exp_tags = exp.tags.clone();
        let mut act_tags = act.tags.clone();
        exp_tags.sort();
        act_tags.sort();
        if exp_tags != act_tags {
            return Err(format!(
                "event {idx} (`{}`): expected tags {exp_tags:?}, got {act_tags:?}",
                exp.event_type
            ));
        }
    }
    Ok(())
}

fn describe_expect(expect: &Expectation) -> String {
    match expect {
        Expectation::Emit(events) => format!("emit of {} event(s)", events.len()),
        Expectation::Reject { code } => format!("reject `{code}`"),
        Expectation::InvalidInput => "invalid_input".to_owned(),
    }
}

fn describe_outcome(outcome: &CommandOutcome) -> String {
    match outcome {
        CommandOutcome::Committed { events, .. } => format!("commit of {} event(s)", events.len()),
        CommandOutcome::Rejected { code, .. } => format!("reject `{code}`"),
        CommandOutcome::InvalidInput { .. } => "invalid_input".to_owned(),
        CommandOutcome::Conflict => "a concurrency conflict".to_owned(),
        CommandOutcome::AlreadyCommitted(_) => "an idempotent replay".to_owned(),
        CommandOutcome::Unavailable { .. } => "the store is unavailable".to_owned(),
    }
}

fn test_files(dir: &Path) -> Vec<PathBuf> {
    WalkDir::new(dir)
        .sort_by_file_name()
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("star"))
        .collect()
}

fn rel_to_string(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
