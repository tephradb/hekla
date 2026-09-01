//! The shadow world: heklang's own host, driven beside hekla's.
//!
//! The oracle for the model test. Everything the interpreter needs from outside itself
//! is answered here by [`heklang::Harness`], which is a complete second implementation
//! of the same four traits hekla implements in `heklang_host`: a log with the real
//! Dynamic Consistency Boundary check, a key store, a clock and a network.
//!
//! Using it rather than writing a model is a structural choice, not a convenience. A
//! hand-written model is at permanent risk of being quietly edited until it agrees with
//! whatever hekla does, and an oracle that agrees by construction proves nothing. This
//! one lives in another repository and was written for another consumer, so bending it
//! to agree would take a cross-repo commit.
//!
//! **What it is not independent of**: heklang's parser, IR and interpreter, which both
//! sides share. A language bug is invisible here. That is the right trade, because the
//! layer under test is hekla's host: conversions, crypto, SQLite, tephra, and the
//! projector and effect drivers.
//!
//! Three things are modelled here rather than borrowed, and each is named where it is
//! written: the key lifecycle as a generation per subject, the effect walk's watermark,
//! and reading a sealed column back. heklang has none of the three, because none of
//! them is a language rule.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use hekla::heklang_host::{from_heklang_json, to_heklang_json};
use hekla::schema::{EntityDef, FieldKind};
use heklang::Row;
use heklang::host::{AppendCondition, Attempt, Clock, Http, Keys, Log, Query, Request, Rows};
use heklang::interp::{Error, Interpreter, Invocation, Projection, Store, key_as_value};
use heklang::ir::Ident;
use heklang::value::{Defs, Event, Json, Key, Record, Value};
use heklang::{Effectful, Harness, Journal, Outcome, Program};

/// The instant every shadow run reads from `now()`.
///
/// Nothing in the fixtures calls it, so this is not a value any assertion depends on:
/// it is here so that a fixture which starts calling it diverges loudly against hekla's
/// wall clock rather than passing by accident.
const SHADOW_NOW: i64 = 1_577_836_800_000_000;

/// A run has to stop somewhere if an effect's `invoke` feeds itself. heklang's own
/// `drive` has the same guard; this walk holds its own cursor and so needs its own.
const MAX_CASCADE: u64 = 4096;

/// One erasure: the subject, and the log head it happened at.
///
/// The head is what makes this a generation rather than a flag. hekla's append path
/// mints a subject key on first use and its projection path only ever uses one that
/// already exists, so content sealed before an erasure is unreadable forever while
/// content sealed after it is readable under new key material. A flag cannot say that;
/// a count of the erasures that preceded a given position can.
#[derive(Debug, Clone)]
struct Erasure {
    subject: String,
    id: String,
    /// The first log position whose content is sealed under the *next* generation.
    at: u64,
}

/// heklang's world, with the three seams hekla's differs on made explicit.
pub struct ShadowHost {
    /// The log and the append condition, borrowed wholesale from heklang.
    log: Harness,
    /// Every erasure so far, in order.
    ///
    /// Held here rather than in the `Harness` (which models the same thing as a flag)
    /// for two reasons. The flag is too coarse, as [`Erasure`] explains; and
    /// [`Interpreter::host`] hands back a shared reference, so an erasure requested from
    /// outside a run has no `&mut` to reach the harness through.
    erasures: RefCell<Vec<Erasure>>,
    /// The position the effect walk is currently delivering, so a `reveal` is answered
    /// against the generation that sealed the content rather than the newest one.
    delivering: Cell<u64>,
    /// What every request answers with, matching the `StubHttpClient` the real side
    /// boots against.
    status: u16,
}

impl ShadowHost {
    pub fn new(status: u16) -> Self {
        Self {
            log: Harness::default(),
            erasures: RefCell::new(Vec::new()),
            delivering: Cell::new(0),
            status,
        }
    }

    /// Destroy a subject's key from outside a run, the way an operator does.
    pub fn shred(&self, subject: &str, id: &str) {
        let at = self.log.records().len() as u64;
        self.erasures.borrow_mut().push(Erasure {
            subject: subject.to_owned(),
            id: id.to_owned(),
            at,
        });
    }

    /// How many times this subject's key had been destroyed by the time `position` was
    /// appended. Content sealed at that position is readable exactly while this still
    /// equals [`generation`](Self::generation).
    fn generation_at(&self, subject: &str, id: &str, position: u64) -> u32 {
        self.erasures
            .borrow()
            .iter()
            .filter(|erasure| {
                erasure.subject == subject && erasure.id == id && erasure.at <= position
            })
            .count() as u32
    }

    /// The generation now in force, which is what a fresh seal is written under.
    fn generation(&self, subject: &str, id: &str) -> u32 {
        self.generation_at(subject, id, u64::MAX)
    }

    pub fn records(&self) -> &[Record] {
        self.log.records()
    }
}

impl Log for ShadowHost {
    fn head(&self) -> Result<u64, Error> {
        self.log.head()
    }

    fn record(&self, position: u64) -> Result<Option<Record>, Error> {
        self.log.record(position)
    }

    fn read(
        &self,
        query: &Query,
        visit: &mut dyn FnMut(&Record) -> Result<(), Error>,
    ) -> Result<(), Error> {
        self.log.read(query, visit)
    }

    fn append(&mut self, events: &[Event], condition: &AppendCondition) -> Result<(), Error> {
        self.log.append(events, condition)
    }
}

impl Clock for ShadowHost {
    fn now(&self) -> i64 {
        SHADOW_NOW
    }
}

/// The lifecycle and nothing else: content sealed under a key that still exists reads
/// back as it was stored, and content sealed under a destroyed one does not read back at
/// all. hekla's is the same question answered with real crypto, which is exactly the
/// difference this comparison exists to find.
///
/// `reveal` reaches this with content bound out of the event at
/// [`delivering`](ShadowHost::delivering), so the generation that sealed it is the one
/// in force at that position rather than the newest one. Without that, an owner erased
/// and then written to again would make an *older* invocation readable.
impl Keys for ShadowHost {
    fn decrypt(
        &self,
        subject: &str,
        id: &str,
        _field: &str,
        content: &str,
    ) -> Result<Option<String>, Error> {
        if self.generation_at(subject, id, self.delivering.get()) != self.generation(subject, id) {
            return Ok(None);
        }
        Ok(Some(content.to_owned()))
    }

    fn erase(&mut self, subject: &str, id: &str) -> Result<(), Error> {
        self.shred(subject, id);
        Ok(())
    }
}

impl Http for ShadowHost {
    /// `StubHttpClient::status` answers every call with `{}` at a fixed status, so this
    /// does too. A response body neither fixture reads, but an unequal one would be a
    /// difference between the worlds rather than a shared constant.
    fn send(&mut self, _request: &Request) -> Attempt {
        Attempt::Response {
            status: self.status,
            body: Json::obj(Vec::<(String, Json)>::new()),
        }
    }
}

/// One shadow run: the interpreter, its effect cursor, and what the effects have done.
pub struct Shadow<'a> {
    interp: Interpreter<'a, ShadowHost>,
    program: &'a Program,
    effect: Option<&'a str>,
    /// The next position the effect walk delivers.
    ///
    /// Held here rather than calling [`Interpreter::drive`], which restarts at position
    /// zero with a fresh journal: driven twice it would perform every call again, which
    /// is the opposite of what the real side does and would make every comparison after
    /// the first one wrong.
    at: u64,
    /// Invocations whose journal came back non-empty, and those it did not. `verify`
    /// replays the first set and skips the second, so these are what its coverage
    /// counters have to equal.
    journaled: usize,
    unjournaled: usize,
    skipped: usize,
}

impl<'a> Shadow<'a> {
    pub fn new(program: &'a Program, effect: Option<&'a str>, status: u16) -> Self {
        Self {
            interp: Interpreter::with_host(program, ShadowHost::new(status)),
            program,
            effect,
            at: 0,
            journaled: 0,
            unjournaled: 0,
            skipped: 0,
        }
    }

    /// Run one command against the shadow, binding its arguments from the same request
    /// body the real side posts.
    ///
    /// The binding is heklang's `Value::from_json`, which is also what hekla's
    /// `dispatch::bind_args` calls, so this is deliberately not an independent
    /// conversion: heklang owns that table and there is no second answer to have. The
    /// table's own round trip is a property test in both repositories.
    pub fn run(&mut self, command: &str, body: &serde_json::Value) -> Result<Outcome, String> {
        let declared = self
            .program
            .command(command)
            .ok_or_else(|| format!("no command `{command}`"))?;
        let defs = Defs::of(self.program);
        let mut args = Vec::with_capacity(declared.params.len());
        for param in &declared.params {
            let json = body.get(&param.name).map_or(Json::Null, to_heklang_json);
            let value = Value::from_json(&json, &param.ty, defs)
                .map_err(|why| format!("`{}`: {why}", param.name))?;
            args.push((param.name.clone(), value));
        }
        // No retry callback: the shadow is single threaded, so nothing can beat it to
        // the log and a conflict here would be a real disagreement rather than a race.
        self.interp
            .run_retrying(command, args, &mut |_| false)
            .map(|execution| execution.outcome)
            .map_err(|err| format!("{err}"))
    }

    pub fn erase(&self, subject: &str, id: &str) {
        self.interp.host().shred(subject, id);
    }

    pub fn log_head(&self) -> u64 {
        self.interp.host().records().len() as u64
    }

    /// Deliver every position the effect has not seen, following the log as an `invoke`
    /// lengthens it. The real side's lane does the same thing on its own thread, which
    /// is why every comparison happens with both sides settled.
    pub fn settle(&mut self) -> Result<(), String> {
        let Some(effect) = self.effect else {
            return Ok(());
        };
        let mut delivered = 0u64;
        while self.at < self.log_head() {
            if delivered > MAX_CASCADE {
                return Err(format!(
                    "effect `{effect}` did not settle after {delivered}"
                ));
            }
            let mut journal = Journal::default();
            self.interp.host().delivering.set(self.at);
            let outcome = self
                .interp
                .deliver(effect, self.at, &mut journal)
                .map_err(|err| format!("delivering {effect} at {}: {err}", self.at))?;
            // An `Ignored` position selected no arm, so there was no invocation to
            // journal and neither side has a row for it to count.
            if !matches!(outcome, Invocation::Ignored) {
                if journal.is_empty() {
                    self.unjournaled += 1;
                } else {
                    self.journaled += 1;
                }
                if matches!(outcome, Invocation::Skipped(_)) {
                    self.skipped += 1;
                }
            }
            self.at += 1;
            delivered += 1;
        }
        Ok(())
    }

    /// How many invocations `verify` should replay, and how many it should skip.
    pub fn invocations(&self) -> (usize, usize) {
        (self.journaled, self.unjournaled)
    }

    /// Invocations rule 12 made terminal, which is what an erasure racing an effect
    /// would change.
    pub fn skipped(&self) -> usize {
        self.skipped
    }

    /// Every request the effects actually sent, in order, as `(url, body)`.
    pub fn requests(&self) -> Vec<(String, serde_json::Value)> {
        self.interp
            .trace()
            .iter()
            .filter_map(|entry| match entry {
                Effectful::Http { url, body, .. } => Some((
                    url.clone(),
                    body.as_ref()
                        .map_or(serde_json::Value::Null, from_heklang_json),
                )),
                _ => None,
            })
            .collect()
    }

    /// One entity's rows, keyed by the rendered key, each row in rule 8's wire form.
    ///
    /// The wire form rather than the column form because that is heklang's own table
    /// and needs no transcription here; [`wire_row`] brings hekla's side to meet it.
    ///
    /// Projected record by record rather than through [`Interpreter::project`], because
    /// the stamp a seal gets is the generation in force at the record that wrote it, and
    /// a fold that hands over only the finished store has thrown that away.
    pub fn rows(&self, projector: &str, entity: &str) -> Result<BTreeMap<String, Fields>, String> {
        let projection = Projection::new(self.program, projector)
            .map_err(|err| format!("projecting {projector}: {err}"))?;
        let host = self.interp.host();
        let mut store = Store::default();
        host.read(&projection.query(), &mut |record| {
            let mut stamping = Stamping {
                store: &mut store,
                host,
                at: record.position,
            };
            projection.apply(record, &mut stamping)
        })
        .map_err(|err| format!("projecting {projector}: {err}"))?;

        let mut out = BTreeMap::new();
        for (key, row) in store.rows(entity) {
            let mut fields = Fields::new();
            for (name, value) in &row.0 {
                if let Some(rendered) = self.render(value) {
                    fields.insert(name.clone(), rendered);
                }
            }
            out.insert(render_key(&key_as_value(key)), fields);
        }
        Ok(out)
    }

    /// One stored column as a reader of the read model would see it.
    ///
    /// The third thing modelled here rather than borrowed. heklang stores a sealed
    /// column as its plaintext behind a `Value::Sealed` and never reads one back, so
    /// "a column whose key generation has moved on reads back absent" is stated here,
    /// once. It is the invariant the whole erasure half of the model test turns on,
    /// which is why it is not spelled anywhere else.
    fn render(&self, value: &Value) -> Option<serde_json::Value> {
        match value {
            Value::Opt { value: None, .. } => None,
            Value::Opt {
                value: Some(inner), ..
            } => self.render(inner),
            Value::Sealed { subject, id, .. } => {
                let (id, sealed_under) = unstamp(id);
                (sealed_under == self.interp.host().generation(subject, id))
                    .then(|| from_heklang_json(&Json::from_value(value)))
            }
            other => match Json::from_value(other) {
                Json::Null => None,
                json => Some(from_heklang_json(&json)),
            },
        }
    }
}

/// The separator between a subject id and the key generation that sealed the content.
///
/// A subject id is a plaintext scalar (an integer or a uuid in the fixtures), so nothing
/// a program can produce contains this.
const STAMP: &str = "#gen";

/// A projection that records which key generation sealed each column it writes.
///
/// The generation travels *on the value* rather than in a table beside it, and that is
/// the whole design. A `patch` or an `update` loads the stored row and writes the
/// untouched columns straight back, so a column can outlive several records; hekla's
/// answer is that the carried ciphertext is still under the key it was sealed with, and
/// re-sealing it re-reads it under the current one. Stamping the value is the same
/// statement: an already-stamped seal keeps its generation, and only a seal fresh out of
/// an event gets the one in force at that event's position.
struct Stamping<'a> {
    store: &'a mut Store,
    host: &'a ShadowHost,
    at: u64,
}

impl Rows for Stamping<'_> {
    fn row(&self, entity: &str, key: &Key) -> Result<Option<Row>, Error> {
        self.store.row(entity, key)
    }

    fn put(&mut self, entity: &Ident, key: Key, mut row: Row) -> Result<(), Error> {
        for value in row.0.values_mut() {
            stamp(value, self.host, self.at);
        }
        self.store.put(entity, key, row)
    }

    fn delete(&mut self, entity: &Ident, key: &Key) -> Result<(), Error> {
        self.store.delete(entity, key)
    }
}

fn stamp(value: &mut Value, host: &ShadowHost, at: u64) {
    match value {
        Value::Opt {
            value: Some(inner), ..
        } => stamp(inner, host, at),
        Value::Sealed { subject, id, .. } if !id.contains(STAMP) => {
            let generation = host.generation_at(subject, id, at);
            id.push_str(STAMP);
            id.push_str(&generation.to_string());
        }
        _ => {}
    }
}

/// A stamped subject id back into the id and the generation. An unstamped one is
/// generation zero, which is what a seal that never met [`Stamping`] is.
fn unstamp(id: &str) -> (&str, u32) {
    match id.split_once(STAMP) {
        Some((id, generation)) => (id, generation.parse().unwrap_or_default()),
        None => (id, 0),
    }
}

/// One row's readable columns. A column that reads back absent is not in the map, on
/// either side, so "absent" and "null" cannot pass for each other.
pub type Fields = BTreeMap<String, serde_json::Value>;

/// A read-model row in rule 8's wire form, so it can be compared against a shadow row.
///
/// Only a `Timestamp` differs between the two forms: it is epoch microseconds on the
/// wire and RFC 3339 in a column, because the read API serves that and the column sorts
/// lexicographically on it. Everything else is already in the right shape, which is why
/// this is two lines rather than a table.
pub fn wire_row(entity: &EntityDef, row: &serde_json::Value) -> Fields {
    let mut out = Fields::new();
    for (name, meta) in &entity.fields {
        let Some(value) = row.get(name).filter(|value| !value.is_null()) else {
            continue;
        };
        let wire = match (meta.kind.base(), value.as_str()) {
            (FieldKind::Timestamp, Some(text)) => heklang::value::timestamp(text)
                .map_or_else(|| value.clone(), serde_json::Value::from),
            _ => value.clone(),
        };
        out.insert(name.clone(), wire);
    }
    out
}

/// A key as the string both sides index their rows by. A `Uuid` key is a JSON string
/// and an `Int` key is a JSON number, and rendering rather than parsing is what keeps
/// the two from colliding.
pub fn render_key(key: &Value) -> String {
    from_heklang_json(&Json::from_value(key)).to_string()
}
