//! `hekla test`: events in, assert what the module did.
//!
//! A test file under `tests/` declares a module-level `cases` list of `case(...)`
//! scenarios. Every case seeds a throwaway store with its `given` events through the
//! same `build_event` path a live append uses, so a seeded event is byte-identical to
//! one the runtime would write, and then drives one module against it:
//!
//! - a **command** runs for real, so `query` filtering and its own `AppendCondition`
//!   are exercised, and `expect` is the events, rejection or invalid input it produced.
//! - a **projector** projects the seeded log into a fresh read model with
//!   [`project_to_head`], and `expect` is the rows [`read_api`] reads back. Going
//!   through the read API rather than the model means a subject-scoped column is
//!   asserted as the plaintext a `GET /read/...` would return.
//! - an **effect** runs its `handle` over each seeded event its keys select, served by
//!   a [`TestEffectHost`] that stubs the responses the case declares and records the
//!   calls made. `expect` is that call sequence. Its boundary folds the same seeded
//!   log, so `given` is the state as well as the trigger.
//!
//! What a case does *not* cover is the machinery around a handler: batching,
//! checkpoints, retry, the journal and replay. Those are the runtime's, and they are
//! covered by the integration tests rather than by a scenario, which keeps a case a
//! statement about the author's own logic.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::fs;
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use allocative::Allocative;
use anyhow::Context;
use starlark::any::ProvidesStaticType;
use starlark::environment::{FrozenModule, Globals, GlobalsBuilder};
use starlark::values::list::{ListRef, UnpackList};
use starlark::values::{NoSerialize, StarlarkValue, Value, ValueLike, starlark_value};
use starlark::{starlark_module, starlark_simple_value};
use tephra::{Position, Query, SegmentConfig, SegmentSet, WriteCoordinator, WriterConfig};
use uuid::Uuid;

use crate::context::{CommandContext, EffectHost};
use crate::crypto::{KeyStore, MasterKeys};
use crate::dispatch::{self, CommandOutcome, EventDefs, build_event};
use crate::effect;
use crate::envelope;
use crate::loader::{
    CommandUnit, EffectUnit, LoadedProject, ProjectorUnit, Severity, rel_to_string, star_files,
};
use crate::opdb::OpDb;
use crate::projector::project_to_head;
use crate::read_api;
use crate::read_model::ReadModel;
use crate::starlark_builtins::{
    ConstructedEvent, EmittedEvent, InvalidInput, ModuleDef, Rejection,
    check_registered_definition, events_from_value, runtime_builtins,
};

/// Throwaway per-case stores stay small, but the segment must still clear the
/// writer's default max batch size.
const TEST_SEGMENT_SIZE: usize = 16 * 1024 * 1024;
/// A fixed clock so a `now()`-using command is reproducible under test.
const TEST_NOW: &str = "1970-01-01T00:00:00Z";
/// A fixed, non-secret master key so `hekla test` can exercise subject-scoped
/// encryption deterministically without any environment setup. Test assertions
/// compare plaintext events, so the ciphertext values never surface.
const TEST_MASTER_KEY: [u8; 32] = [0x2a; 32];

/// The event id of the nth `given` event, counting from 1.
///
/// Fixed rather than random for the same reason the clock and the master key are:
/// a handler may now read `event.id` and derive an id from it with `uuid5`, and a
/// case asserting on that derived id has to get the same answer on every run. The
/// low-numbered form is also writable by hand, so a test can name the id it expects
/// (`"00000000-0000-0000-0000-000000000001"` is the first `given` event).
fn seeded_event_id(index: usize) -> Uuid {
    Uuid::from_u128(index as u128 + 1)
}

/// Which module kind a scenario drives, and the inputs only that kind takes.
#[derive(Debug, Clone, Allocative)]
enum Target {
    /// Run the command with this input (its JSON wire form).
    Command { name: String, input: String },
    /// Project `given` into a fresh read model and read the rows back.
    Projector { name: String },
    /// Run the effect's `handle` over `given`, serving `responds` to its HTTP calls.
    /// State comes from `given` too: the effect's boundary folds the same seeded log.
    Effect {
        name: String,
        responds: Vec<ResponseStub>,
    },
}

impl Target {
    fn name(&self) -> &str {
        match self {
            Target::Command { name, .. }
            | Target::Projector { name }
            | Target::Effect { name, .. } => name,
        }
    }
}

/// What a scenario expects to happen.
#[derive(Debug, Clone, Allocative)]
enum Expectation {
    /// These events, in order (compared by type, data and tags).
    Emit(Vec<ConstructedEvent>),
    /// A state-dependent rejection with this code (the message is not asserted).
    Reject { code: String },
    /// A malformed-input rejection (the message is not asserted).
    InvalidInput,
    /// Entity name to its rows, as the read API returns them. The JSON wire form,
    /// since a `serde_json::Value` is not `Allocative`.
    Rows(String),
    /// The external calls an effect makes, in order.
    Calls(Vec<ExpectedCall>),
}

/// One `case(...)` scenario.
#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct TestCase {
    name: Option<String>,
    target: Target,
    given: Vec<ConstructedEvent>,
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
            None => self.target.name().to_owned(),
        }
    }
}

#[starlark_value(type = "test_case")]
impl<'v> StarlarkValue<'v> for TestCase {}
starlark_simple_value!(TestCase);

/// A stubbed HTTP response, served to an effect's calls in the order they are made.
#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct ResponseStub {
    status: u16,
    /// The response body as JSON text, which is what the transport carries.
    body: String,
    headers: Vec<(String, String)>,
}

impl fmt::Display for ResponseStub {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "http_response(status = {})", self.status)
    }
}

#[starlark_value(type = "http_response")]
impl<'v> StarlarkValue<'v> for ResponseStub {}
starlark_simple_value!(ResponseStub);

/// One external call a scenario expects, in the order the handler makes them.
///
/// Only the fields a case names are compared, so a case that cares about the URL need
/// not restate the headers, and one that cares about the body need not restate either.
#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) enum ExpectedCall {
    Http {
        method: Option<String>,
        url: String,
        /// JSON text, or `None` to not compare the body at all.
        body: Option<String>,
    },
    Command {
        name: String,
        input: String,
    },
    Erase {
        subject_field: String,
        subject_value: String,
    },
}

impl fmt::Display for ExpectedCall {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ExpectedCall::Http { method, url, .. } => {
                write!(f, "{} {url}", method.as_deref().unwrap_or("any"))
            }
            ExpectedCall::Command { name, .. } => write!(f, "invoke_command(\"{name}\")"),
            ExpectedCall::Erase {
                subject_field,
                subject_value,
            } => write!(f, "erase(\"{subject_field}\", \"{subject_value}\")"),
        }
    }
}

#[starlark_value(type = "expected_call")]
impl<'v> StarlarkValue<'v> for ExpectedCall {}
starlark_simple_value!(ExpectedCall);

/// One external call an effect actually made, recorded by the test host.
#[derive(Debug, Clone)]
pub(crate) enum MadeCall {
    Http {
        method: String,
        url: String,
        body: serde_json::Value,
    },
    Command {
        name: String,
        input: serde_json::Value,
    },
    Erase {
        subject_field: String,
        subject_value: String,
    },
}

impl fmt::Display for MadeCall {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            MadeCall::Http { method, url, .. } => write!(f, "{method} {url}"),
            MadeCall::Command { name, .. } => write!(f, "invoke_command(\"{name}\")"),
            MadeCall::Erase {
                subject_field,
                subject_value,
            } => write!(f, "erase(\"{subject_field}\", \"{subject_value}\")"),
        }
    }
}

/// The `case(...)` builtin exposed to test files, and the values its arguments take.
#[starlark_module]
pub(crate) fn test_builtins(builder: &mut GlobalsBuilder) {
    /// Declare a scenario. Exactly one of `command`, `projector` or `effect` names
    /// what to run; `given` are the prior events, constructed from event definitions.
    ///
    /// - `command`: `input` is the input dict, `expect` is an event, a list of them,
    ///   `reject(...)` or `invalid_input(...)`.
    /// - `projector`: `given` is projected into a fresh read model, and `expect` is a
    ///   dict of entity name to the rows the read API should return.
    /// - `effect`: `handle` runs over each `given` event, `responds` serves its HTTP
    ///   calls in order, and `expect` is the list of `http_call(...)`,
    ///   `command_call(...)` and `erase_call(...)` it should have made. An effect's
    ///   state needs nothing extra: its boundary folds the same seeded `given`.
    // One argument per authored keyword: all named, all optional bar `expect`, so
    // grouping them into a struct would move the surface rather than shrink it.
    #[allow(clippy::too_many_arguments)]
    fn case<'v>(
        #[starlark(require = named)] expect: Value<'v>,
        #[starlark(require = named)] command: Option<String>,
        #[starlark(require = named)] projector: Option<String>,
        #[starlark(require = named)] effect: Option<String>,
        #[starlark(require = named)] input: Option<Value<'v>>,
        #[starlark(require = named)] responds: Option<UnpackList<Value<'v>>>,
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
        let target = parse_target(command, projector, effect, input, responds)?;
        let expect = parse_expectation(expect, &target)?;
        Ok(TestCase {
            name,
            target,
            given,
            expect,
        })
    }

    /// A stubbed HTTP response for an effect case. `responds` serves these to the
    /// handler's `http.*` calls in the order it makes them.
    fn http_response(
        #[starlark(require = named)] status: u32,
        #[starlark(require = named)] body: Option<Value<'_>>,
        #[starlark(require = named)] headers: Option<Value<'_>>,
    ) -> anyhow::Result<ResponseStub> {
        // The runtime absorbs every retryable status and retries it itself, so none
        // reaches a handler. A case that declared one would be describing a path
        // that cannot happen.
        if !(100..600).contains(&status) || effect::is_retryable_status(status as u16) {
            anyhow::bail!(
                "http_response() status must be one a handler can actually see; the runtime retries 408, 425, 429 and every 5xx itself, so none of those ever reaches one"
            );
        }
        let body = match body {
            Some(value) => value
                .to_json_value()
                .map_err(|err| anyhow::anyhow!("http_response() body must be JSON: {err}"))?
                .to_string(),
            None => "null".to_owned(),
        };
        Ok(ResponseStub {
            status: status as u16,
            body,
            headers: header_pairs(headers)?,
        })
    }

    /// An HTTP request an effect case expects. Only the arguments given are compared.
    fn http_call(
        #[starlark(require = named)] url: String,
        #[starlark(require = named)] method: Option<String>,
        #[starlark(require = named)] body: Option<Value<'_>>,
    ) -> anyhow::Result<ExpectedCall> {
        let body = match body {
            Some(value) => Some(
                value
                    .to_json_value()
                    .map_err(|err| anyhow::anyhow!("http_call() body must be JSON: {err}"))?
                    .to_string(),
            ),
            None => None,
        };
        Ok(ExpectedCall::Http {
            method: method.map(|m| m.to_ascii_uppercase()),
            url,
            body,
        })
    }

    /// An `invoke_command` an effect case expects.
    fn command_call<'v>(
        #[starlark(require = pos)] name: String,
        #[starlark(require = pos)] input: Value<'v>,
    ) -> anyhow::Result<ExpectedCall> {
        let input = input
            .to_json_value()
            .map_err(|err| anyhow::anyhow!("command_call() input must be JSON: {err}"))?;
        if !input.is_object() {
            anyhow::bail!("command_call() input must be a dict");
        }
        Ok(ExpectedCall::Command {
            name,
            input: input.to_string(),
        })
    }

    /// An `erase` an effect case expects, naming the subject whose key it deleted.
    fn erase_call(
        #[starlark(require = pos)] subject_field: String,
        #[starlark(require = pos)] subject_value: String,
    ) -> anyhow::Result<ExpectedCall> {
        Ok(ExpectedCall::Erase {
            subject_field,
            subject_value,
        })
    }
}

/// Read the `headers = {...}` argument of `http_response`, which is a plain
/// `str: str` dict: a stub serves one value per header, unlike the multimap a real
/// response can carry.
fn header_pairs(headers: Option<Value<'_>>) -> anyhow::Result<Vec<(String, String)>> {
    let Some(value) = headers else {
        return Ok(Vec::new());
    };
    let json = value
        .to_json_value()
        .map_err(|err| anyhow::anyhow!("http_response() headers must be JSON: {err}"))?;
    let obj = json
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("http_response() headers must be a dict of str: str"))?;
    obj.iter()
        .map(|(name, value)| match value.as_str() {
            Some(text) => Ok((name.clone(), text.to_owned())),
            None => anyhow::bail!("http_response() header `{name}` must be a string"),
        })
        .collect()
}

/// Pick the one target a case names, and reject the combinations that would leave the
/// runner guessing.
fn parse_target(
    command: Option<String>,
    projector: Option<String>,
    effect: Option<String>,
    input: Option<Value<'_>>,
    responds: Option<UnpackList<Value<'_>>>,
) -> anyhow::Result<Target> {
    let named: Vec<&str> = [
        command.as_ref().map(|_| "command"),
        projector.as_ref().map(|_| "projector"),
        effect.as_ref().map(|_| "effect"),
    ]
    .into_iter()
    .flatten()
    .collect();
    let [kind] = named[..] else {
        anyhow::bail!(
            "a case must name exactly one of `command`, `projector` or `effect`, got {}",
            if named.is_empty() {
                "none".to_owned()
            } else {
                named.join(" and ")
            }
        );
    };
    if input.is_some() && kind != "command" {
        anyhow::bail!("`input` is a command's request payload, so a {kind} case takes none");
    }
    if responds.is_some() && kind != "effect" {
        anyhow::bail!("`responds` stubs an effect's http calls, so a {kind} case takes none");
    }
    match (command, projector, effect) {
        (Some(name), _, _) => {
            let input = input
                .ok_or_else(|| anyhow::anyhow!("a command case must give `input`"))?
                .to_json_value()
                .map_err(|err| anyhow::anyhow!("input must be JSON-serialisable: {err}"))?;
            if !input.is_object() {
                anyhow::bail!("input must be a dict");
            }
            Ok(Target::Command {
                name,
                input: input.to_string(),
            })
        }
        (_, Some(name), _) => Ok(Target::Projector { name }),
        (_, _, Some(name)) => {
            let responds = responds
                .map(|list| {
                    list.items
                        .iter()
                        .map(|item| {
                            item.downcast_ref::<ResponseStub>().cloned().ok_or_else(|| {
                                anyhow::anyhow!(
                                    "responds items must be http_response(...), got {}",
                                    item.get_type()
                                )
                            })
                        })
                        .collect::<anyhow::Result<Vec<_>>>()
                })
                .transpose()?
                .unwrap_or_default();
            Ok(Target::Effect { name, responds })
        }
        _ => unreachable!("exactly one target was checked above"),
    }
}

/// Read `expect` in the shape the case's target calls for.
///
/// The target decides, rather than the value's own type, because the shapes overlap:
/// an empty list is "no events" for a command and "no calls" for an effect, and
/// nothing about `[]` says which. Knowing the target also lets each mismatch name the
/// form that kind actually takes.
fn parse_expectation(value: Value<'_>, target: &Target) -> anyhow::Result<Expectation> {
    match target {
        Target::Command { .. } => parse_command_expectation(value),
        Target::Projector { .. } => {
            let json = value.to_json_value().map_err(|err| {
                anyhow::anyhow!("a projector case's `expect` must be JSON-serialisable: {err}")
            })?;
            if !json.is_object() {
                anyhow::bail!(
                    "a projector case's `expect` is a dict of entity name to its rows, got {}",
                    value.get_type()
                );
            }
            Ok(Expectation::Rows(json.to_string()))
        }
        Target::Effect { .. } => Ok(Expectation::Calls(expected_calls(value)?)),
    }
}

fn parse_command_expectation(value: Value<'_>) -> anyhow::Result<Expectation> {
    if let Some(reject) = value.downcast_ref::<Rejection>() {
        return Ok(Expectation::Reject {
            code: reject.code.clone(),
        });
    }
    if value.downcast_ref::<InvalidInput>().is_some() {
        return Ok(Expectation::InvalidInput);
    }
    if let Some(events) = events_from_value(value)? {
        return Ok(Expectation::Emit(events));
    }
    anyhow::bail!(
        "a command case's `expect` must be an event, a list of events, reject(...) or invalid_input(...), got {}",
        value.get_type()
    )
}

/// The calls an effect case expects, in order. An empty list is meaningful here: it
/// asserts the handler reached nothing external, which is what an unselected event and
/// an early return both look like from outside.
fn expected_calls(value: Value<'_>) -> anyhow::Result<Vec<ExpectedCall>> {
    if let Some(call) = value.downcast_ref::<ExpectedCall>() {
        return Ok(vec![call.clone()]);
    }
    let list = ListRef::from_value(value).ok_or_else(|| {
        anyhow::anyhow!(
            "an effect case's `expect` is a list of http_call(...) and command_call(...), got {}",
            value.get_type()
        )
    })?;
    list.iter()
        .map(|item| {
            item.downcast_ref::<ExpectedCall>().cloned().ok_or_else(|| {
                anyhow::anyhow!(
                    "an effect case's `expect` holds only http_call(...) and command_call(...), got {}",
                    item.get_type()
                )
            })
        })
        .collect()
}

/// Globals for test files: the base builtins (so `reject`/`invalid_input` and
/// event constructors work) plus `case(...)`.
pub(crate) fn test_globals() -> Globals {
    GlobalsBuilder::standard()
        .with(runtime_builtins)
        .with(test_builtins)
        .build()
}

/// Run `hekla test` over the project at `dir`.
pub fn run(dir: &Path) -> ExitCode {
    let project = LoadedProject::load(dir);
    // Reuse the CLI's collection so `hekla test` reports the same findings, in the
    // same location order, as `hekla check` and `hekla serve`.
    let findings = crate::cli::collect_findings(&project);
    let errors: Vec<_> = findings
        .iter()
        .filter(|finding| finding.severity == Severity::Error)
        .collect();
    if !errors.is_empty() {
        for finding in &errors {
            eprintln!("error: {}: {}", finding.location, finding.message);
        }
        eprintln!(
            "cannot run tests: the project has {} error(s)",
            errors.len()
        );
        return ExitCode::FAILURE;
    }

    let globals = test_globals();
    let mut passed = 0usize;
    let mut failed = 0usize;

    let tests_dir = dir.join("tests");
    let mut walk_findings = Vec::new();
    let test_paths = if tests_dir.is_dir() {
        star_files(&tests_dir, &mut walk_findings)
    } else {
        Vec::new()
    };
    for finding in &walk_findings {
        eprintln!("error: {}: {}", finding.location, finding.message);
        failed += 1;
    }

    for path in test_paths {
        let rel = rel_to_string(dir, &path);
        let src = match fs::read_to_string(&path) {
            Ok(src) => src,
            Err(err) => {
                eprintln!("error: {rel}: reading file: {err}");
                failed += 1;
                continue;
            }
        };
        // Test files call event definitions in `given`/`expect` to construct
        // events, not to filter, so this is not query mode.
        let module = match project.eval_against_libraries(&rel, src, &globals, false) {
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
pub(crate) fn read_cases(module: &FrozenModule) -> anyhow::Result<Vec<TestCase>> {
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
    match execute_case(project, case) {
        Ok(outcome) => compare(&case.expect, &outcome),
        Err(err) => Err(format!("{err:#}")),
    }
}

/// A throwaway store seeded with a case's `given` events, plus the key store that
/// encrypted them. Shared by all three kinds: a projector reads the same log a command
/// appends to, so seeding must go through one path.
struct Seeded {
    _dir: tempfile::TempDir,
    coordinator: WriteCoordinator,
    store: tephra::WriteHandle,
    keystore: KeyStore,
}

fn seed(case: &TestCase, events: &EventDefs) -> anyhow::Result<Seeded> {
    check_case_definitions(case, events)?;
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
    for (index, event) in case.given.iter().enumerate() {
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
            seeded_event_id(index),
        )?);
    }
    if !packed.is_empty() {
        store
            .append(packed, None)
            .map_err(|err| anyhow::anyhow!("seeding given events: {err}"))?;
    }
    Ok(Seeded {
        _dir: dir,
        coordinator,
        store,
        keystore,
    })
}

/// What a case actually did, in the shape its `expect` is compared against.
enum Outcome {
    Command(CommandOutcome),
    /// Entity name to the rows the read API returned, entity order as declared.
    Rows(Vec<(String, Vec<serde_json::Value>)>),
    Calls(Vec<MadeCall>),
}

fn execute_case(project: &LoadedProject, case: &TestCase) -> anyhow::Result<Outcome> {
    let events = &project.events.by_type;
    let seeded = seed(case, events)?;
    let outcome = match &case.target {
        Target::Command { name, input } => {
            let command = project
                .commands
                .iter()
                .find(|unit| unit.loaded.def.name() == name)
                .ok_or_else(|| anyhow::anyhow!("no command named `{name}`"))?;
            run_command_case(&seeded, command, events, input).map(Outcome::Command)
        }
        Target::Projector { name } => {
            let projector = project
                .projectors
                .iter()
                .find(|unit| unit.loaded.def.name() == name)
                .ok_or_else(|| anyhow::anyhow!("no projector named `{name}`"))?;
            run_projector_case(&seeded, projector, events).map(Outcome::Rows)
        }
        Target::Effect { name, responds } => {
            let effect = project
                .effects
                .iter()
                .find(|unit| unit.loaded.def.name() == name)
                .ok_or_else(|| anyhow::anyhow!("no effect named `{name}`"))?;
            run_effect_case(&seeded, effect, events, responds).map(Outcome::Calls)
        }
    };
    seeded.coordinator.shutdown();
    outcome
}

fn run_command_case(
    seeded: &Seeded,
    command: &CommandUnit,
    events: &EventDefs,
    input: &str,
) -> anyhow::Result<CommandOutcome> {
    let input: serde_json::Value = serde_json::from_str(input).context("parsing the case input")?;
    let ctx = CommandContext::new(Uuid::new_v4());
    // Host-side validation is a first-class outcome, and the runtime performs it
    // once before dispatch; mirror that split here so tests exercise the same path.
    Ok(match dispatch::validate_input(&command.loaded, &input) {
        Ok(()) => dispatch::run_command(
            &seeded.store,
            &command.loaded,
            events,
            Some(&seeded.keystore),
            &input,
            &ctx,
            TEST_NOW,
            None,
            true,
            &dispatch::Retry::once(),
        )?,
        Err(err) => CommandOutcome::InvalidInput {
            message: format!("{err}"),
        },
    })
}

/// Project the seeded log into a fresh read model and read every entity back.
///
/// Rows come through [`read_api::scan`] with the case's key store rather than off the
/// model directly, so a subject-scoped column reads as the plaintext a
/// `GET /read/...` would return instead of stored ciphertext.
fn run_projector_case(
    seeded: &Seeded,
    projector: &ProjectorUnit,
    events: &EventDefs,
) -> anyhow::Result<Vec<(String, Vec<serde_json::Value>)>> {
    let ModuleDef::Projector { entities, .. } = &projector.loaded.def else {
        anyhow::bail!("not a projector");
    };
    let dir = tempfile::tempdir().context("creating a temp read model")?;
    let path = dir.path().join("model.db");
    let model = ReadModel::open(&path, entities).context("opening the temp read model")?;
    project_to_head(&seeded.store, &projector.loaded, &model, events)?;
    drop(model);

    let mut out = Vec::with_capacity(entities.len());
    for entity in entities {
        let page = read_api::scan(
            &path,
            entity,
            None,
            None,
            read_api::MAX_LIMIT,
            Some(&seeded.keystore),
        )
        .with_context(|| format!("reading entity `{}`", entity.name))?;
        out.push((entity.name.clone(), page.items));
    }
    Ok(out)
}

/// Serves an effect's impure builtins from a case's declarations and records every
/// external call it makes.
///
/// There is no journal: a case runs a handler once, so there is nothing to replay.
/// The host serves only the genuinely impure builtins; state reaches the handler
/// through its boundary, folded off the same seeded log the case built from `given`.
struct TestEffectHost<'a> {
    keystore: &'a KeyStore,
    responds: &'a [ResponseStub],
    /// Advanced per HTTP call, so `responds` is consumed in the order made.
    served: Cell<usize>,
    calls: RefCell<Vec<MadeCall>>,
}

impl EffectHost for TestEffectHost<'_> {
    fn http(
        &self,
        method: &str,
        url: &str,
        _headers: Vec<(String, String)>,
        body: Option<serde_json::Value>,
    ) -> anyhow::Result<serde_json::Value> {
        let index = self.served.get();
        self.served.set(index + 1);
        self.calls.borrow_mut().push(MadeCall::Http {
            method: method.to_owned(),
            url: url.to_owned(),
            body: body.unwrap_or(serde_json::Value::Null),
        });
        let stub = self.responds.get(index).ok_or_else(|| {
            anyhow::anyhow!(
                "the handler made {} http call(s) but the case declares {} response(s); add another `http_response(...)` to `responds`",
                index + 1,
                self.responds.len()
            )
        })?;
        let headers: serde_json::Map<String, serde_json::Value> = stub
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    name.clone(),
                    serde_json::Value::Array(vec![serde_json::Value::String(value.clone())]),
                )
            })
            .collect();
        let body: serde_json::Value =
            serde_json::from_str(&stub.body).unwrap_or(serde_json::Value::Null);
        Ok(serde_json::json!({
            "status": stub.status,
            "body": body,
            "headers": headers,
        }))
    }

    fn invoke_command(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        self.calls.borrow_mut().push(MadeCall::Command {
            name: name.to_owned(),
            input: input.clone(),
        });
        // The command is recorded, not run: a case asserts that the effect asked for
        // it, and the command's own behaviour belongs in a command case.
        Ok(serde_json::json!({ "status": 200, "body": serde_json::Value::Null }))
    }

    fn now(&self) -> anyhow::Result<String> {
        Ok(TEST_NOW.to_owned())
    }

    fn log(&self, _message: &str) {}

    /// Recorded like the other side effects, and really performed against the case's
    /// own key store, so a `reveal()` after an `erase()` in the same handler fails
    /// exactly as it would live.
    fn erase(&self, subject_field: &str, subject_value: &str) -> anyhow::Result<bool> {
        self.calls.borrow_mut().push(MadeCall::Erase {
            subject_field: subject_field.to_owned(),
            subject_value: subject_value.to_owned(),
        });
        self.keystore.erase(subject_field, subject_value)
    }

    fn reveal(
        &self,
        subject_field: &str,
        subject_value: &str,
        field: &str,
        ciphertext: &str,
    ) -> anyhow::Result<String> {
        self.keystore
            .decrypt_subject(subject_field, subject_value, field, ciphertext)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "reveal() cannot decrypt `{field}`: subject `{subject_field}` = `{subject_value}` has been erased"
                )
            })
    }
}

/// Run the effect's `handle` over every seeded event its keys select, recording the
/// external calls.
///
/// Events are read back out of the store rather than replayed from `given`, so an arm
/// matches against the same lowered `QueryItem` and the same encrypted tags the live
/// subscription would see. That same store is what an effect's boundary folds, so
/// `given` is both the trigger and the state with nothing further to declare.
fn run_effect_case(
    seeded: &Seeded,
    effect: &EffectUnit,
    events: &EventDefs,
    responds: &[ResponseStub],
) -> anyhow::Result<Vec<MadeCall>> {
    let host = TestEffectHost {
        keystore: &seeded.keystore,
        responds,
        served: Cell::new(0),
        calls: RefCell::new(Vec::new()),
    };
    let mut positions = Vec::new();
    let mut reads = seeded.store.read(&Query::All, Position::ZERO, None);
    while let Some(item) = reads.next() {
        let seq = item.map_err(|err| anyhow::anyhow!("reading seeded events: {err}"))?;
        positions.push((seq.position.get(), seq.event.to_owned()));
    }
    // Collected first: the fold opens its own read of the same store, and holding the
    // outer lending iterator across it would borrow the store twice.
    for (position, event) in &positions {
        let (envelope, data) = envelope::decode(event.data())
            .map_err(|err| anyhow::anyhow!("reading event: {err}"))?;
        let inv = effect::Invocation {
            events,
            store: &seeded.store,
            keystore: Some(&seeded.keystore),
            position: *position,
            event,
            env: &envelope,
            event_type: event.event_type(),
            data: &data,
            // A scenario is cheap and is exactly where a nondeterministic fold should
            // surface first, so `hekla test` always checks.
            verify: true,
        };
        effect::run_handle(&effect.loaded, &inv, &host)?;
    }
    Ok(host.calls.into_inner())
}

fn check_case_definitions(case: &TestCase, events: &EventDefs) -> anyhow::Result<()> {
    let expected: &[ConstructedEvent] = match &case.expect {
        Expectation::Emit(list) => list,
        Expectation::Reject { .. }
        | Expectation::InvalidInput
        | Expectation::Rows(_)
        | Expectation::Calls(_) => &[],
    };
    for event in case.given.iter().chain(expected) {
        check_registered_definition(event, events)?;
    }
    Ok(())
}

fn compare(expect: &Expectation, outcome: &Outcome) -> Result<(), String> {
    match (expect, outcome) {
        (Expectation::Rows(expected), Outcome::Rows(actual)) => compare_rows(expected, actual),
        (Expectation::Calls(expected), Outcome::Calls(actual)) => compare_calls(expected, actual),
        (expect, Outcome::Command(outcome)) => compare_command(expect, outcome),
        (expect, outcome) => Err(format!(
            "expected {}, got {}",
            describe_expect(expect),
            describe_actual(outcome)
        )),
    }
}

fn compare_command(expect: &Expectation, outcome: &CommandOutcome) -> Result<(), String> {
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

/// Compare the read model against the declared rows, entity by entity.
///
/// Only the entities a case names are checked, so a projector with several tables can
/// be asserted one at a time. Rows come back in key order from the read API, so a case
/// does not depend on the order events were projected in.
fn compare_rows(expected: &str, actual: &[(String, Vec<serde_json::Value>)]) -> Result<(), String> {
    let expected: serde_json::Value =
        serde_json::from_str(expected).map_err(|err| format!("parsing expected rows: {err}"))?;
    let expected = expected.as_object().expect("checked to be an object");
    for (entity, want) in expected {
        let Some((_, got)) = actual.iter().find(|(name, _)| name == entity) else {
            let known: Vec<&str> = actual.iter().map(|(name, _)| name.as_str()).collect();
            return Err(format!(
                "the projector declares no entity `{entity}`; it has {known:?}"
            ));
        };
        let want = want
            .as_array()
            .ok_or_else(|| format!("expect[`{entity}`] must be a list of rows"))?;
        if want.len() != got.len() {
            return Err(format!(
                "entity `{entity}`: expected {} row(s), got {}: {}",
                want.len(),
                got.len(),
                serde_json::Value::Array(got.clone())
            ));
        }
        for (idx, (want, got)) in want.iter().zip(got).enumerate() {
            if want != got {
                return Err(format!(
                    "entity `{entity}` row {idx}: expected {want}, got {got}"
                ));
            }
        }
    }
    Ok(())
}

/// Compare the calls an effect made against the ones declared, in order.
///
/// An expected call compares only the fields it names, so a case that cares about the
/// URL need not restate the body.
fn compare_calls(expected: &[ExpectedCall], actual: &[MadeCall]) -> Result<(), String> {
    if expected.len() != actual.len() {
        let made: Vec<String> = actual.iter().map(|call| call.to_string()).collect();
        return Err(format!(
            "expected {} call(s), got {}: {made:?}",
            expected.len(),
            actual.len()
        ));
    }
    for (idx, (want, got)) in expected.iter().zip(actual).enumerate() {
        match (want, got) {
            (
                ExpectedCall::Http { method, url, body },
                MadeCall::Http {
                    method: got_method,
                    url: got_url,
                    body: got_body,
                },
            ) => {
                if let Some(method) = method
                    && method != got_method
                {
                    return Err(format!(
                        "call {idx}: expected method `{method}`, got `{got_method}`"
                    ));
                }
                if url != got_url {
                    return Err(format!("call {idx}: expected url `{url}`, got `{got_url}`"));
                }
                if let Some(body) = body {
                    let want: serde_json::Value = serde_json::from_str(body)
                        .map_err(|err| format!("call {idx}: parsing expected body: {err}"))?;
                    if &want != got_body {
                        return Err(format!("call {idx}: expected body {want}, got {got_body}"));
                    }
                }
            }
            (
                ExpectedCall::Command { name, input },
                MadeCall::Command {
                    name: got_name,
                    input: got_input,
                },
            ) => {
                if name != got_name {
                    return Err(format!(
                        "call {idx}: expected invoke_command(`{name}`), got (`{got_name}`)"
                    ));
                }
                let want: serde_json::Value = serde_json::from_str(input)
                    .map_err(|err| format!("call {idx}: parsing expected input: {err}"))?;
                if &want != got_input {
                    return Err(format!(
                        "call {idx}: expected input {want}, got {got_input}"
                    ));
                }
            }
            (
                ExpectedCall::Erase {
                    subject_field,
                    subject_value,
                },
                MadeCall::Erase {
                    subject_field: got_field,
                    subject_value: got_value,
                },
            ) => {
                if subject_field != got_field || subject_value != got_value {
                    return Err(format!(
                        "call {idx}: expected erase(`{subject_field}`, `{subject_value}`), got (`{got_field}`, `{got_value}`)"
                    ));
                }
            }
            (want, got) => return Err(format!("call {idx}: expected {want}, got {got}")),
        }
    }
    Ok(())
}

fn describe_actual(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Command(outcome) => describe_outcome(outcome),
        Outcome::Rows(_) => "read-model rows".to_owned(),
        Outcome::Calls(calls) => format!("{} external call(s)", calls.len()),
    }
}

fn describe_expect(expect: &Expectation) -> String {
    match expect {
        Expectation::Emit(events) => format!("append of {} event(s)", events.len()),
        Expectation::Reject { code } => format!("reject `{code}`"),
        Expectation::InvalidInput => "invalid_input".to_owned(),
        Expectation::Rows(_) => "read-model rows".to_owned(),
        Expectation::Calls(calls) => format!("{} external call(s)", calls.len()),
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
