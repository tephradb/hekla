//! The stateful model test: a sequence of operations run against a real runtime and
//! against heklang's own world, with every observable compared after each one.
//!
//! `verify::sweep` compares hekla against hekla, so a deterministic wrong answer is
//! invisible to it: `rebuild_equivalence` rebuilds with the same code that built the
//! live model. This is the layer that answers a different question. The shadow world in
//! `support::shadow` is a second implementation of the same host traits, in another
//! repository, written for another consumer, so a disagreement between the two is a bug
//! in one of them rather than a rounding of the same mistake twice.
//!
//! Four surfaces are compared after every operation, and the fifth is deliberately not:
//!
//! | Surface | How |
//! | --- | --- |
//! | Command outcome | the HTTP status and refusal code against heklang's `Outcome` |
//! | Read model rows | `read_api::scan` with decryption against the shadow's own projection |
//! | Effect traffic | `StubHttpClient::calls` against `Interpreter::trace` |
//! | Log length | both heads, which is event conservation |
//! | **Stored event bytes** | **never**: heklang models ciphertext as plaintext and synthesises envelopes, so the comparison would be meaningless |
//!
//! No oracle here cross-checks `lower` or `record_of`, and a symmetric bug in either
//! stays invisible. That is the honest limit of this file.
//!
//! Checkpoint monotonicity is deliberately **not** asserted here, and the reason is
//! worth writing down because an earlier draft did assert it. Nothing a sequence can do
//! makes a checkpoint move backwards, so the assertion passed against a projector with
//! no guard at all; and `reset_position` is documented to publish a lower checkpoint
//! after a rebuild, so asserting the sample never falls claims more than the design
//! promises. It belongs where the guard is, in `projector::tests`.
//!
//! **Anti-transcription rule**: no change to the shadow's own three modelled facts (the
//! key lifecycle, the effect watermark, and reading a sealed column back) may share a
//! commit with the source change it was found by. A model edited to agree is not an
//! oracle.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hekla::effect::StubHttpClient;
use hekla::read_api;
use hekla::runtime::ExecResult;
use heklang::Outcome;
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::FileFailurePersistence;
use proptest::test_runner::TestRunner;
use serde_json::{Value, json};
use tempfile::TempDir;

mod support;

use support::shadow::{Fields, Shadow, wire_row};
use support::{Boot, Harness, ctx, example_dir, fixture_dir, replay_and_wait, sweep, try_quiesce};

/// The status the stub answers with on both sides. A 2xx, so the tickets effect gets
/// past its status guard and reaches the `invoke` that appends.
const HTTP_STATUS: u16 = 200;

/// Where a found counterexample is written, so it becomes a permanent case.
///
/// Named explicitly because proptest's default looks for the source root beside the
/// test file and an integration test has none: left alone it prints a warning and
/// persists nothing, which is the difference between a bug found once and a bug found
/// once and then kept.
const REGRESSIONS: FileFailurePersistence =
    FileFailurePersistence::Direct("proptest-regressions/model.txt");

// --- the operations -------------------------------------------------------

/// One step of a case.
///
/// Every index is taken modulo a fixed universe rather than into a live set, so any
/// subsequence a shrinker produces is still meaningful and no operation becomes
/// unreachable once an earlier one is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Create {
        thing: u8,
        owner: u8,
        group: u8,
        contact: bool,
    },
    Relabel {
        thing: u8,
        label: u8,
    },
    Remove {
        thing: u8,
        group: u8,
    },
    EraseOwner(u8),
    EraseGroup(u8),
    Restart,
    Rebuild {
        projector: u8,
    },
    Verify,
}

/// The fixed universe every index is folded into.
const THINGS: u8 = 12;
const OWNERS: u8 = 4;
const GROUPS: u8 = 3;

/// What one operation means to a scenario's world.
pub enum Act {
    Run {
        command: &'static str,
        body: Value,
    },
    Erase {
        subject: &'static str,
        id: String,
    },
    /// The scenario has no such operation. `examples/orders` writes once and never
    /// updates or deletes, so a `Relabel` there is nothing rather than a command, and
    /// counting it says which shapes a project actually reaches.
    Nothing,
}

/// A project, plus what the operations mean in it.
///
/// A struct of plain data and one function pointer rather than a trait, because the only
/// thing that varies between scenarios is the vocabulary: `tickets` and `orders` disagree
/// about what a `Create` is and agree about everything else.
pub struct Scenario {
    pub name: &'static str,
    /// The project hekla boots.
    pub dir: PathBuf,
    /// The project the shadow interprets, normally the same one.
    ///
    /// Two fields rather than one because both worlds run the same `.hk` source through
    /// heklang, so mutating the fixture moves both of them and proves nothing. Pointing
    /// them at different copies is what makes the comparison demonstrably
    /// shadow-against-hekla rather than hekla against itself, and
    /// [`a_projector_that_counts_differently_on_one_side_is_caught`] is the test that
    /// uses it.
    pub model_dir: PathBuf,
    /// The one effect lane the shadow drives, if the project has one.
    pub effect: Option<&'static str>,
    pub projectors: &'static [&'static str],
    /// The `(projector, entity)` pairs whose rows are compared.
    pub entities: &'static [(&'static str, &'static str)],
    pub act: fn(&Op) -> Act,
}

/// The `tickets` fixture: four write statements, two subjects on one event, an
/// `Int @key`, a per-organisation cap, and an effect that posts and then invokes.
pub fn tickets() -> Scenario {
    Scenario {
        name: "tickets",
        dir: fixture_dir("tickets"),
        model_dir: fixture_dir("tickets"),
        effect: Some("NotifyOwner"),
        projectors: &["Tickets"],
        entities: &[("Tickets", "Ticket"), ("Tickets", "OrgTotals")],
        act: |op| match *op {
            Op::Create {
                thing,
                owner,
                group,
                contact,
            } => {
                let priority = ["Low", "Normal", "Urgent"][usize::from(thing) % 3];
                // Distinct per thing, so a timestamp column that stopped varying would
                // show up as a row difference rather than as nothing.
                let due_at = 1_700_000_000_000_000i64 + i64::from(thing) * 86_400_000_000;
                let contact = contact.then(|| format!("owner{owner}@example.com"));
                Act::Run {
                    command: "OpenTicket",
                    body: json!({
                        "ticket_id": ticket_id(thing),
                        "org_id": org_id(group),
                        "owner_id": owner_id(owner),
                        "title": format!("ticket {thing}"),
                        "priority": priority,
                        "due_at": due_at,
                        "fee": "12.50",
                        "budget": "900.00",
                        "contact": contact,
                        "meta": { "thing": thing, "note": "generated" },
                    }),
                }
            }
            Op::Relabel { thing, label } => Act::Run {
                command: "RetitleTicket",
                body: json!({ "ticket_id": ticket_id(thing), "title": format!("relabel {label}") }),
            },
            Op::Remove { thing, group } => Act::Run {
                command: "CloseTicket",
                body: json!({ "ticket_id": ticket_id(thing), "org_id": org_id(group) }),
            },
            Op::EraseOwner(owner) => Act::Erase {
                subject: "owner_id",
                id: owner_id(owner).to_string(),
            },
            Op::EraseGroup(group) => Act::Erase {
                subject: "org_id",
                id: org_id(group).to_string(),
            },
            Op::Restart | Op::Rebuild { .. } | Op::Verify => {
                unreachable!("a world operation never reaches the scenario")
            }
        },
    }
}

/// `examples/orders`: the canonical example, run through the same machinery.
///
/// A deliberately different shape from `tickets`, which is the point of running it at
/// all. One write statement instead of four, so `Relabel` and `Remove` are nothing here;
/// a cap of three rather than six, so a sequence spends most of its time in the refusal
/// branch; two subjects again, but the shop's sealed column is not projected at all,
/// while both of the customer's are; and an effect that folds its own state and posts
/// without invoking, so nothing cascades and the log length is exactly the successes.
pub fn orders() -> Scenario {
    Scenario {
        name: "orders",
        dir: example_dir("orders"),
        model_dir: example_dir("orders"),
        effect: Some("NotifyCustomer"),
        projectors: &["CustomerOrders"],
        entities: &[("CustomerOrders", "Order")],
        act: |op| match *op {
            Op::Create {
                thing,
                owner,
                group,
                contact,
            } => {
                let email = contact.then(|| format!("owner{owner}@example.com"));
                let address = contact.then(|| format!("{owner} Test Street"));
                Act::Run {
                    command: "PlaceOrder",
                    body: json!({
                        "order_id": ticket_id(thing),
                        "customer_id": owner_id(owner),
                        "shop_id": org_id(group),
                        "email": email,
                        "shipping_address": address,
                        "order_total": format!("{}.50", 10 + thing),
                        "notes": format!("order {thing}"),
                    }),
                }
            }
            Op::EraseOwner(owner) => Act::Erase {
                subject: "customer_id",
                id: owner_id(owner).to_string(),
            },
            Op::EraseGroup(group) => Act::Erase {
                subject: "shop_id",
                id: org_id(group).to_string(),
            },
            // An order is placed once and never changed: there is no command to run.
            Op::Relabel { .. } | Op::Remove { .. } => Act::Nothing,
            Op::Restart | Op::Rebuild { .. } | Op::Verify => {
                unreachable!("a world operation never reaches the scenario")
            }
        },
    }
}
fn ticket_id(thing: u8) -> String {
    uuid::Uuid::from_u128(u128::from(thing % THINGS) + 1).to_string()
}

fn owner_id(owner: u8) -> i64 {
    10 + i64::from(owner % OWNERS)
}

fn org_id(group: u8) -> i64 {
    1 + i64::from(group % GROUPS)
}

// --- what a case produces -------------------------------------------------

/// How often each operation class reached each outcome.
///
/// A histogram rather than a set of named counters, so it does not have to know what a
/// scenario's outcomes are called. Its job is to make a degenerate universe visible: a
/// `Create` that always hits the cap is a no-op, and a case where every operation is a
/// no-op is the most likely way this whole effort produces green tests that check
/// nothing.
#[derive(Debug, Clone, Default)]
pub struct Coverage {
    pub hits: BTreeMap<String, usize>,
}

impl Coverage {
    fn bump(&mut self, what: impl Into<String>) {
        *self.hits.entry(what.into()).or_default() += 1;
    }

    pub fn count(&self, what: &str) -> usize {
        self.hits.get(what).copied().unwrap_or_default()
    }

    /// Fold another case's histogram in, so a generated run reports what the whole run
    /// reached rather than what its last case did.
    pub fn absorb(&mut self, other: &Coverage) {
        for (what, count) in &other.hits {
            *self.hits.entry(what.clone()).or_default() += count;
        }
    }
}

impl fmt::Display for Coverage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (what, count) in &self.hits {
            writeln!(f, "  {what}: {count}")?;
        }
        Ok(())
    }
}

/// What the two worlds disagreed about.
#[derive(Debug, Clone)]
pub enum Disagreement {
    /// The command answered differently.
    Outcome { real: String, shadow: String },
    /// One entity's rows differ. Rendered as the whole keyed set rather than the first
    /// differing row, because "a row that should not exist" and "a row that should"
    /// are the same failure seen from two sides.
    Rows {
        entity: &'static str,
        key: String,
        real: Option<Fields>,
        shadow: Option<Fields>,
    },
    /// The effect sent different traffic.
    Requests {
        real: Vec<(String, Value)>,
        shadow: Vec<(String, Value)>,
    },
    /// Different numbers of events, which no row comparison can see on its own.
    LogHead { real: u64, shadow: u64 },
    /// An invocation was terminally skipped, which under this file's quiescing
    /// discipline nothing should reach. `tests/concurrent.rs` is where the race that
    /// legitimately produces one is asserted.
    Skipped { real: u64, shadow: usize },
    /// The offline sweep found something, or covered less than the shadow says it
    /// should have.
    Sweep(String),
    /// A lane did not settle inside the poll budget. Not a comparison failure, but it
    /// has to come back as a result rather than a panic so a planted violation that
    /// wedges a lane is still reportable.
    Stalled(&'static str),
    /// The oracle itself could not answer. Kept apart from every variant above so a
    /// broken shadow cannot read as a caught bug.
    Oracle(String),
}

/// A disagreement, with everything needed to reproduce it.
#[derive(Debug, Clone)]
pub struct Divergence {
    pub scenario: &'static str,
    pub ops: Vec<Op>,
    pub at: usize,
    pub coverage: Coverage,
    pub what: Disagreement,
}

impl fmt::Display for Divergence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "the shadow world disagreed on `{}` at op {} of {}",
            self.scenario,
            self.at,
            self.ops.len()
        )?;
        for (index, op) in self.ops.iter().enumerate() {
            let marker = if index == self.at { "->" } else { "  " };
            writeln!(f, "{marker} {index:>3}: {op:?}")?;
        }
        match &self.what {
            Disagreement::Outcome { real, shadow } => {
                writeln!(f, "outcome: hekla said {real}, the shadow said {shadow}")?;
            }
            Disagreement::Rows {
                entity,
                key,
                real,
                shadow,
            } => {
                writeln!(f, "row {entity}[{key}]:")?;
                writeln!(f, "  hekla:  {}", render_row(real.as_ref()))?;
                writeln!(f, "  shadow: {}", render_row(shadow.as_ref()))?;
            }
            Disagreement::Requests { real, shadow } => {
                writeln!(f, "requests:")?;
                writeln!(f, "  hekla:  {real:?}")?;
                writeln!(f, "  shadow: {shadow:?}")?;
            }
            Disagreement::LogHead { real, shadow } => {
                writeln!(f, "log head: hekla {real}, the shadow {shadow}")?;
            }
            Disagreement::Skipped { real, shadow } => {
                writeln!(f, "terminal skips: hekla {real}, the shadow {shadow}")?;
            }
            Disagreement::Sweep(why) => writeln!(f, "sweep: {why}")?,
            Disagreement::Stalled(what) => writeln!(f, "stalled: {what}")?,
            Disagreement::Oracle(why) => writeln!(f, "the oracle failed: {why}")?,
        }
        write!(f, "coverage:\n{}", self.coverage)
    }
}

fn render_row(fields: Option<&Fields>) -> String {
    match fields {
        Some(fields) => format!("{fields:?}"),
        None => "<no row>".to_owned(),
    }
}

// --- the interpreter ------------------------------------------------------

/// Run one case through both worlds.
///
/// **Every disagreement is returned, never panicked.** That is what lets a
/// planted-violation test exist: it corrupts something, calls this on a fixed op list,
/// and asserts a specific [`Disagreement`]. A helper that panicked from inside would
/// make that untestable, and a lane that wedges under a planted fault comes back as
/// [`Disagreement::Stalled`] rather than as a timeout inside a wait helper.
///
/// A project that will not load or a runtime that will not boot still panics. Those are
/// a broken setup rather than a disagreement, and reporting them as one would put a
/// mistake in the test where a bug in hekla is supposed to go.
pub fn run_case(scenario: &Scenario, ops: &[Op]) -> Result<Coverage, Box<Divergence>> {
    let data = match tempfile::tempdir() {
        Ok(data) => data,
        Err(err) => {
            return Err(oracle(
                scenario,
                ops,
                0,
                Coverage::default(),
                err.to_string(),
            ));
        }
    };
    let stub = Arc::new(StubHttpClient::status(HTTP_STATUS));
    let program = support::load_ok(&scenario.model_dir).program;

    let mut run = Run {
        scenario,
        ops: ops.to_vec(),
        coverage: Coverage::default(),
        harness: Some(boot(scenario, data.path(), &stub)),
        shadow: Shadow::new(&program, scenario.effect, HTTP_STATUS),
        stub,
        data: &data,
    };
    let outcome = run.drive();
    if let Some(harness) = run.harness.take() {
        harness.shutdown();
    }
    outcome
}

fn boot(scenario: &Scenario, data: &Path, stub: &Arc<StubHttpClient>) -> Harness {
    Boot::new(&scenario.dir)
        .data_dir(data)
        .with_master_key()
        .http(Arc::clone(stub) as Arc<dyn hekla::http::HttpClient>)
        .start()
}

struct Run<'a> {
    scenario: &'a Scenario,
    ops: Vec<Op>,
    coverage: Coverage,
    /// An `Option` only so a restart can take the live runtime out to shut it down;
    /// it holds a runtime everywhere a step can observe it.
    harness: Option<Harness>,
    shadow: Shadow<'a>,
    stub: Arc<StubHttpClient>,
    data: &'a TempDir,
}

impl Run<'_> {
    fn drive(&mut self) -> Result<Coverage, Box<Divergence>> {
        for at in 0..self.ops.len() {
            self.step(at).map_err(|what| {
                Box::new(Divergence {
                    scenario: self.scenario.name,
                    ops: self.ops.clone(),
                    at,
                    coverage: self.coverage.clone(),
                    what,
                })
            })?;
        }
        Ok(self.coverage.clone())
    }

    fn step(&mut self, at: usize) -> Result<(), Disagreement> {
        let op = self.ops[at];
        match op {
            Op::Restart => {
                self.settle()?;
                self.reboot();
                self.coverage.bump("restart");
            }
            Op::Rebuild { projector } => {
                self.settle()?;
                let name = self.scenario.projectors
                    [usize::from(projector) % self.scenario.projectors.len()];
                let rt = self.rt()?;
                replay_and_wait(&rt, name);
                self.coverage.bump("rebuild");
            }
            Op::Verify => {
                self.settle()?;
                self.verify()?;
                self.coverage.bump("verify");
            }
            _ => match (self.scenario.act)(&op) {
                Act::Run { command, body } => self.run_command(&op, command, &body)?,
                Act::Erase { subject, id } => {
                    // Quiesced first, deliberately. An erase racing a pending effect
                    // has two legitimate outcomes, delivered or terminally skipped,
                    // which is a set-level property and not a model assertion; it gets
                    // its own test rather than being smuggled in here.
                    self.settle()?;
                    let rt = self.rt()?;
                    let keystore = rt
                        .keystore()
                        .ok_or_else(|| Disagreement::Oracle("no key store".to_owned()))?;
                    keystore
                        .erase(subject, &id)
                        .map_err(|err| Disagreement::Oracle(format!("erasing: {err}")))?;
                    self.shadow.erase(subject, &id);
                    self.coverage.bump(format!("erase:{subject}"));
                }
                Act::Nothing => self.coverage.bump(format!("{}:unsupported", class_of(&op))),
            },
        }
        self.settle()?;
        self.compare()
    }

    fn run_command(&mut self, op: &Op, command: &str, body: &Value) -> Result<(), Disagreement> {
        let real = self
            .rt()?
            .execute(command, body.clone(), &ctx(), None)
            .map_err(|err| Disagreement::Oracle(format!("executing {command}: {err}")))?;
        let shadow = self
            .shadow
            .run(command, body)
            .map_err(|why| Disagreement::Oracle(format!("shadow {command}: {why}")))?;

        let (real, shadow) = (classify_real(&real), classify_shadow(&shadow));
        if real != shadow {
            return Err(Disagreement::Outcome { real, shadow });
        }
        self.coverage.bump(format!("{}:{real}", class_of(op)));
        Ok(())
    }

    fn live(&self) -> Result<&Harness, Disagreement> {
        self.harness
            .as_ref()
            .ok_or_else(|| Disagreement::Oracle("the runtime is not booted".to_owned()))
    }

    fn rt(&self) -> Result<Arc<hekla::runtime::Runtime>, Disagreement> {
        Ok(Arc::clone(&self.live()?.rt))
    }

    /// Both worlds to a standstill. The shadow's effect walk is synchronous, so this is
    /// what puts the two logs in the same order: hekla's lane finishes an operation's
    /// effects before the next operation appends, which is the order the shadow walks
    /// in by construction.
    fn settle(&mut self) -> Result<(), Disagreement> {
        if !try_quiesce(self.live()?) {
            return Err(Disagreement::Stalled("the runtime did not quiesce"));
        }
        self.shadow.settle().map_err(Disagreement::Oracle)
    }

    fn reboot(&mut self) {
        if let Some(stale) = self.harness.take() {
            stale.shutdown();
        }
        self.harness = Some(boot(self.scenario, self.data.path(), &self.stub));
    }

    /// A `Verify` is shutdown, sweep, reboot, because `verify::sweep` takes the
    /// data-directory lock. The cost is real and buys restart coverage on every verify.
    fn verify(&mut self) -> Result<(), Disagreement> {
        if let Some(stale) = self.harness.take() {
            stale.shutdown();
        }
        let report = sweep(&self.scenario.dir, self.data.path());
        self.harness = Some(boot(self.scenario, self.data.path(), &self.stub));
        let (journaled, unjournaled) = self.shadow.invocations();

        // Equalities against what the shadow delivered, never floors. `is_clean` is only
        // `violations.is_empty()`, so a sweep that skipped everything reads exactly like
        // a clean one, and every silent skip path inside `sweep_effect` is a number that
        // would stop matching here.
        if !report.is_clean() {
            return Err(Disagreement::Sweep(format!("{:?}", report.violations)));
        }
        if report.projectors_checked != self.scenario.projectors.len() {
            return Err(Disagreement::Sweep(format!(
                "checked {} projector(s), the project declares {}",
                report.projectors_checked,
                self.scenario.projectors.len()
            )));
        }
        if report.invocations_checked != journaled {
            return Err(Disagreement::Sweep(format!(
                "replayed {} invocation(s), the shadow journaled {journaled}",
                report.invocations_checked
            )));
        }
        if report.invocations_skipped != unjournaled {
            return Err(Disagreement::Sweep(format!(
                "skipped {} invocation(s), the shadow journaled nothing for {unjournaled}",
                report.invocations_skipped
            )));
        }
        Ok(())
    }

    fn compare(&mut self) -> Result<(), Disagreement> {
        let real_head = self.rt()?.log_head();
        let shadow_head = self.shadow.log_head();
        if real_head != shadow_head {
            return Err(Disagreement::LogHead {
                real: real_head,
                shadow: shadow_head,
            });
        }

        for (projector, entity) in self.scenario.entities {
            let real = self.real_rows(projector, entity)?;
            let shadow = self
                .shadow
                .rows(projector, entity)
                .map_err(Disagreement::Oracle)?;
            let keys: BTreeSet<&String> = real.keys().chain(shadow.keys()).collect();
            for key in keys {
                if real.get(key) != shadow.get(key) {
                    return Err(Disagreement::Rows {
                        entity,
                        key: key.clone(),
                        real: real.get(key).cloned(),
                        shadow: shadow.get(key).cloned(),
                    });
                }
            }
        }

        let real = self.real_requests();
        let shadow = self.shadow.requests();
        if real != shadow {
            return Err(Disagreement::Requests { real, shadow });
        }

        // Both zero, not merely equal. A deterministic sequence quiesces before every
        // erase, so nothing here should ever reveal a shredded key; the assertion is
        // that the discipline held, and a skip on either side means it stopped. Equality
        // alone would not say that, and hekla's counter is per boot while the shadow's
        // is per run, so it could not say it after a restart anyway.
        let skipped = self.shadow.skipped();
        let terminal: u64 = self
            .scenario
            .effect
            .and_then(|name| self.live().ok()?.rt.effect(name))
            .map_or(0, |shared| shared.terminal_skips());
        if skipped != 0 || terminal != 0 {
            return Err(Disagreement::Skipped {
                real: terminal,
                shadow: skipped,
            });
        }

        Ok(())
    }

    /// One entity's rows through the read API, which decrypts, so both sides are
    /// plaintext. Paged to exhaustion: a truncated page would read as a missing row.
    fn real_rows(
        &self,
        projector: &str,
        entity: &str,
    ) -> Result<BTreeMap<String, Fields>, Disagreement> {
        let rt = self.rt()?;
        let shared = rt
            .projector(projector)
            .ok_or_else(|| Disagreement::Oracle(format!("no projector `{projector}`")))?;
        let def = read_api::find_entity(&shared.entities, entity)
            .ok_or_else(|| Disagreement::Oracle(format!("no entity `{entity}`")))?;

        let mut out = BTreeMap::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = read_api::scan(
                &shared.db_path,
                def,
                None,
                cursor.as_deref(),
                64,
                rt.keystore(),
            )
            .map_err(|err| Disagreement::Oracle(format!("scanning {entity}: {err}")))?;
            for row in &page.items {
                let key = row
                    .get(&def.key)
                    .ok_or_else(|| Disagreement::Oracle(format!("{entity} row has no key")))?;
                out.insert(key.to_string(), wire_row(def, row));
            }
            match page.next_cursor {
                Some(next) => {
                    cursor = Some(
                        read_api::decode_cursor(&next)
                            .map_err(|err| Disagreement::Oracle(format!("{err}")))?,
                    );
                }
                None => break,
            }
        }
        Ok(out)
    }

    /// Every request the effect lane actually sent, in order. The same `Arc` is carried
    /// across every reboot, so this is the whole run's traffic and a restart that
    /// re-delivered would show up as a duplicate.
    fn real_requests(&self) -> Vec<(String, Value)> {
        self.stub
            .calls()
            .into_iter()
            .map(|call| {
                let body = call
                    .body
                    .as_deref()
                    .and_then(|bytes| serde_json::from_slice(bytes).ok())
                    .unwrap_or(Value::Null);
                (call.url, body)
            })
            .collect()
    }
}

fn oracle(
    scenario: &Scenario,
    ops: &[Op],
    at: usize,
    coverage: Coverage,
    why: String,
) -> Box<Divergence> {
    Box::new(Divergence {
        scenario: scenario.name,
        ops: ops.to_vec(),
        at,
        coverage,
        what: Disagreement::Oracle(why),
    })
}

fn class_of(op: &Op) -> &'static str {
    match op {
        Op::Create { .. } => "create",
        Op::Relabel { .. } => "relabel",
        Op::Remove { .. } => "remove",
        Op::EraseOwner(_) => "erase-owner",
        Op::EraseGroup(_) => "erase-group",
        Op::Restart => "restart",
        Op::Rebuild { .. } => "rebuild",
        Op::Verify => "verify",
    }
}

/// hekla's answer, as the shape the shadow can be compared against: the status, and for
/// a commit the event types it emitted, and for a refusal its code.
fn classify_real(result: &ExecResult) -> String {
    match result.status {
        200 => {
            let events = result.body["events"]
                .as_array()
                .map(|events| {
                    events
                        .iter()
                        .filter_map(|event| event["type"].as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            format!("ok[{events}]")
        }
        422 => format!(
            "reject:{}",
            result.body["error"]["code"].as_str().unwrap_or("?")
        ),
        400 => "invalid".to_owned(),
        other => format!("status:{other}"),
    }
}

/// The same shape from heklang's own outcome. The event path is written with a leading
/// `@` in the language and without one on the wire, which is the only thing normalised.
fn classify_shadow(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Ok(events) => {
            let events = events
                .iter()
                .map(|event| event.path.to_string().trim_start_matches('@').to_owned())
                .collect::<Vec<_>>()
                .join(",");
            format!("ok[{events}]")
        }
        Outcome::Reject { code, .. } => format!("reject:{code}"),
        Outcome::Invalid(_) => "invalid".to_owned(),
    }
}

// --- the cases ------------------------------------------------------------

/// A hand-written sequence reaching every operation class, so the machinery above is
/// known good before a generator points it anywhere.
///
/// Written down rather than generated on purpose: this is the case a human can read, and
/// the one [`a_projector_that_counts_differently_on_one_side_is_caught`] runs against.
#[rustfmt::skip]
fn walkthrough() -> Vec<Op> {
    vec![
        Op::Create { thing: 0, owner: 0, group: 0, contact: true },
        Op::Create { thing: 1, owner: 1, group: 0, contact: false },
        Op::Create { thing: 2, owner: 0, group: 1, contact: true },
        // A repeat of an open ticket, which the narrow slice makes a no-op.
        Op::Create { thing: 0, owner: 0, group: 0, contact: true },
        Op::Relabel { thing: 0, label: 7 },
        // A retitle of a ticket that was never opened, which `update` declines.
        Op::Relabel { thing: 9, label: 3 },
        Op::Rebuild { projector: 0 },
        Op::Remove { thing: 1, group: 0 },
        // Closing something that was never opened, which is a refusal.
        Op::Remove { thing: 8, group: 2 },
        Op::Verify,
        Op::EraseOwner(0),
        Op::Restart,
        Op::EraseGroup(1),
        // Live subjects, so this one runs: it is what proves the erasures above did not
        // simply stop the fixture from working.
        Op::Create { thing: 3, owner: 2, group: 2, contact: true },
        // The same owner, whose key was destroyed two steps ago. hekla mints a fresh
        // one on the append and the old ticket stays shredded, which is the case the
        // shadow models as a generation rather than a flag.
        Op::Create { thing: 4, owner: 0, group: 2, contact: true },
        Op::Rebuild { projector: 0 },
        Op::Verify,
    ]
}

/// The two worlds agree on a sequence that reaches every operation class, and the
/// coverage histogram says so rather than the absence of a failure saying it.
#[test]
fn the_two_worlds_agree_on_a_walkthrough() {
    let coverage = run_case(&tickets(), &walkthrough()).unwrap_or_else(|why| panic!("{why}"));

    // Equalities, not floors. A sequence that quietly stopped reaching the interesting
    // branches would still pass a floor, and a degenerate universe is the likeliest way
    // this file goes green while checking nothing.
    for (what, want) in [
        ("create:ok[ticket.opened]", 5),
        ("create:ok[]", 1),
        ("relabel:ok[ticket.retitled]", 1),
        ("relabel:reject:no_such_ticket", 1),
        ("remove:ok[ticket.closed]", 1),
        ("remove:reject:no_such_ticket", 1),
        ("erase:owner_id", 1),
        ("erase:org_id", 1),
        ("restart", 1),
        ("rebuild", 2),
        ("verify", 2),
    ] {
        assert_eq!(
            coverage.count(what),
            want,
            "`{what}` fired {} time(s), expected {want}:\n{coverage}",
            coverage.count(what)
        );
    }
}

/// An organisation running out of its allocation, which is the branch a long sequence
/// spends most of its time in and the one a cap-sized universe has to reach.
#[test]
fn the_two_worlds_agree_when_an_allocation_runs_out() {
    let mut ops: Vec<Op> = (0..8)
        .map(|thing| Op::Create {
            thing,
            owner: thing,
            group: 0,
            contact: thing % 2 == 0,
        })
        .collect();
    ops.push(Op::Verify);

    let coverage = run_case(&tickets(), &ops).unwrap_or_else(|why| panic!("{why}"));
    assert_eq!(
        coverage.count("create:ok[ticket.opened]"),
        6,
        "the fixture's cap is six:\n{coverage}"
    );
    assert_eq!(
        coverage.count("create:reject:org_full"),
        2,
        "and the two past it are refused:\n{coverage}"
    );
}

/// The row comparison is shadow against hekla, not hekla against itself.
///
/// This is the case the rest of the file rests on. A comparison that agreed by
/// construction would pass every test above while proving nothing, so one side gets a
/// projector that counts by two and the other keeps the one that counts by one, and the
/// result has to be a `Rows` disagreement naming `OrgTotals`: not a stall, not a clean
/// run, and not a difference somewhere else.
///
/// Mutating the fixture in place would prove nothing, which is worth saying plainly:
/// both worlds interpret the same `.hk` source through heklang, so a change to it moves
/// them together. Only feeding them different programs separates them.
#[test]
fn a_projector_that_counts_differently_on_one_side_is_caught() {
    const COUNTS_BY_ONE: &str =
        "patch OrgTotals[org_id] { opened: .opened + 1, spend: .spend + fee }";
    const COUNTS_BY_TWO: &str =
        "patch OrgTotals[org_id] { opened: .opened + 2, spend: .spend + fee }";

    let copy = tempfile::tempdir().unwrap();
    copy_tree(&fixture_dir("tickets"), copy.path());

    let projector = copy.path().join("projectors/tickets.hk");
    let source = std::fs::read_to_string(&projector).unwrap();
    let mutated = source.replace(COUNTS_BY_ONE, COUNTS_BY_TWO);
    assert_ne!(
        mutated, source,
        "the fixture's patch statement has moved, so this test is no longer mutating anything"
    );
    std::fs::write(&projector, mutated).unwrap();

    let mut scenario = tickets();
    scenario.dir = copy.path().to_path_buf();

    let divergence = run_case(&scenario, &walkthrough())
        .expect_err("a projector counting by two should disagree with one counting by one");
    assert!(
        matches!(
            divergence.what,
            Disagreement::Rows {
                entity: "OrgTotals",
                ..
            }
        ),
        "expected an OrgTotals row disagreement, got {divergence}"
    );
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in walkdir::WalkDir::new(from) {
        let entry = entry.unwrap();
        let target = to.join(entry.path().strip_prefix(from).unwrap());
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
        } else {
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

// --- generated sequences --------------------------------------------------

/// One operation, weighted so the sequence does something rather than mostly refusing.
///
/// `Create` is heaviest because everything else needs a row to act on, and `Verify` is
/// lightest because it is the only operation that costs a shutdown, a sweep and a boot.
/// Every index is bounded by the fixed universe rather than taken modulo at use, so a
/// shrunk value still names something the scenario can reach.
fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0..THINGS, 0..OWNERS, group(), any::<bool>()).prop_map(
            |(thing, owner, group, contact)| Op::Create { thing, owner, group, contact }
        ),
        3 => (0..THINGS, 0..8u8).prop_map(|(thing, label)| Op::Relabel { thing, label }),
        3 => (0..THINGS, group()).prop_map(|(thing, group)| Op::Remove { thing, group }),
        1 => (0..OWNERS).prop_map(Op::EraseOwner),
        1 => (0..GROUPS).prop_map(Op::EraseGroup),
        1 => Just(Op::Restart),
        2 => Just(Op::Rebuild { projector: 0 }),
        2 => Just(Op::Verify),
    ]
}

/// The always-on generated case.
///
/// Measured at about 12 seconds for the whole file on this machine, against a budget of
/// thirty for everything phase 4 adds. A boot is a tempdir plus tephra plus SQLite plus
/// threads and a `Verify` is a shutdown, a sweep and a boot, so the case count is what
/// the budget buys rather than a number chosen for its own sake.
///
/// Shrinking is bounded for the same reason: each shrink step re-runs a whole case, and
/// a report that arrives is worth more than a minimal one that does not.
#[test]
fn generated_sequences_agree_with_the_shadow_world() {
    let seen = Mutex::new(Coverage::default());
    let config = ProptestConfig {
        cases: 8,
        max_shrink_iters: 96,
        max_shrink_time: 30_000,
        failure_persistence: Some(Box::new(REGRESSIONS)),
        ..ProptestConfig::default()
    };
    proptest!(config, |(ops in prop::collection::vec(op(), 6..14))| {
        match run_case(&tickets(), &ops) {
            Ok(coverage) => seen.lock().unwrap().absorb(&coverage),
            Err(why) => prop_assert!(false, "{why}"),
        }
    });

    let seen = seen.into_inner().unwrap();
    // The one floor here rather than an equality, because what a random sequence
    // reaches is not a fact to pin. It guards the way this test would go quietly
    // useless: a generator or a fixture that stopped producing anything runnable would
    // pass every comparison by having nothing to compare.
    assert!(
        seen.count("create:ok[ticket.opened]") > 0,
        "not one generated sequence opened a ticket:\n{seen}"
    );
}

/// The generator reaches every operation class.
///
/// Sampled rather than run, so it costs nothing and can afford enough draws to be a
/// statement about the strategy instead of about one seed. A weight that fell to zero
/// in an edit is invisible to the case above, which would still pass with an operation
/// it never produced.
#[test]
fn the_generator_reaches_every_op_class() {
    let mut runner = TestRunner::deterministic();
    let strategy = op();
    let mut seen: BTreeSet<&'static str> = BTreeSet::new();
    for _ in 0..1_024 {
        let op = strategy.new_tree(&mut runner).unwrap().current();
        seen.insert(class_of(&op));
    }
    let want: BTreeSet<&'static str> = [
        "create",
        "relabel",
        "remove",
        "erase-owner",
        "erase-group",
        "restart",
        "rebuild",
        "verify",
    ]
    .into_iter()
    .collect();
    assert_eq!(
        seen, want,
        "the generator does not reach every operation class"
    );
}

/// The deliberate run: longer sequences, more of them, and an assertion that every
/// outcome a scenario can produce was actually produced.
///
/// Ignored by default because it is minutes rather than seconds. It is the test that
/// says the universe is contended enough to be worth generating over: a fixture that
/// stopped reaching its cap or its refusals would leave the always-on case above green
/// and empty, and only a count of what fired can say so.
///
/// `HEKLA_SOAK_CASES` and `HEKLA_SOAK_OPS` widen it without an edit, which is what a
/// deliberate long run wants: the defaults are what a developer will sit through, not
/// what the question "have we stopped finding bugs" deserves.
fn soak(scenario: &Scenario, required: &[&str]) {
    let cases = env_or("HEKLA_SOAK_CASES", 64);
    let ops = env_or("HEKLA_SOAK_OPS", 48);
    let seen = Mutex::new(Coverage::default());
    let config = ProptestConfig {
        cases,
        max_shrink_iters: 96,
        max_shrink_time: 120_000,
        failure_persistence: Some(Box::new(REGRESSIONS)),
        ..ProptestConfig::default()
    };
    proptest!(config, |(ops in prop::collection::vec(op(), (ops / 2) as usize..ops as usize))| {
        match run_case(scenario, &ops) {
            Ok(coverage) => seen.lock().unwrap().absorb(&coverage),
            Err(why) => prop_assert!(false, "{why}"),
        }
    });

    let seen = seen.into_inner().unwrap();
    // Printed before the assertions, so a run that fails one of them still says what it
    // reached. Someone who has just waited an hour for this wants the histogram either
    // way, and a class that fired twice in ten thousand operations is worth noticing
    // before it becomes a class that fires never.
    println!("what the `{}` soak reached:\n{seen}", scenario.name);
    for what in required {
        assert!(seen.count(what) > 0, "`{what}` never fired:\n{seen}");
    }
}

fn env_or(name: &str, fallback: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(fallback)
}

/// Four write statements, an `Int @key`, a cap of six, and an effect that invokes.
#[test]
#[ignore = "a deliberate soak, minutes rather than seconds"]
fn a_soak_of_the_tickets_fixture_reaches_every_outcome() {
    soak(
        &tickets(),
        &[
            "create:ok[ticket.opened]",
            "create:ok[]",
            "create:reject:org_full",
            "relabel:ok[ticket.retitled]",
            "relabel:reject:no_such_ticket",
            "remove:ok[ticket.closed]",
            "remove:reject:no_such_ticket",
            "erase:owner_id",
            "erase:org_id",
            "restart",
            "rebuild",
            "verify",
        ],
    );
}

/// The canonical example, whose shape is nothing like the fixture's.
///
/// One write statement, a cap of three so most of a sequence is refusals, a sealed
/// column that no projector stores, and an effect that folds its own state and posts
/// without invoking. Everything the machinery claims should hold here too, and until
/// this existed every generated-sequence guarantee was a claim about one project.
#[test]
#[ignore = "a deliberate soak, minutes rather than seconds"]
fn a_soak_of_the_orders_example_reaches_every_outcome() {
    soak(
        &orders(),
        &[
            "create:ok[order.placed]",
            "create:ok[]",
            "create:reject:sold_out",
            "relabel:unsupported",
            "remove:unsupported",
            "erase:customer_id",
            "erase:shop_id",
            "restart",
            "rebuild",
            "verify",
        ],
    );
}

/// Which organisation an operation names, biased towards the first.
///
/// Uniform over three groups spread twelve things too thin ever to fill one: a soak of
/// sixty-four long cases never once reached the cap, which left the refusal branch of
/// the command untested by every generated sequence. The bias is what makes contention
/// a thing the generator produces rather than a thing the fixture merely permits.
fn group() -> impl Strategy<Value = u8> {
    prop_oneof![2 => Just(0u8), 1 => 0..GROUPS]
}
