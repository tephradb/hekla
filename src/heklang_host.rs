//! The world hekla hands heklang: tephra for the log, the key store for subjects,
//! an HTTP client for the network, and the operational database for a journal.
//!
//! Everything here is a conversion. heklang decides what a program means and this
//! decides what that costs in storage, so the two models meet exactly once, at this
//! seam, and neither reshapes itself to suit the other.
//!
//! **Crypto lives below this file, and a ciphertext crosses it.** heklang's
//! `Value::Sealed` carries what this stored rather than the plaintext, so [`Log::read`]
//! decrypts nothing: it hands the ciphertext through and [`Keys::decrypt`] opens it at
//! the one `reveal` that asks for it. [`Log::append`] still seals, because encrypting is
//! the direction that has the content in hand.
//!
//! Reading used to decrypt every subject-scoped field of every record a fold walked,
//! which cost 3.5µs a record and made a fold four times its own cost for content nothing
//! read (`tests/measure.rs`). It also needed a placeholder for a shredded key, so that a
//! field stayed *present* and heklang's rule 12 could keep absent and erased apart.
//! Carrying the ciphertext deletes both.

use anyhow::anyhow;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::hash::{Hash, Hasher};
use std::slice;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use heklang::host::{
    AppendCondition, Attempt, Calls, Clock, Http, Keys, Log, Predicate, Query, Recorded, Request,
    Secrets,
};
use heklang::interp::{Error, ErrorKind};
use heklang::ir::{EventPath, RecordDef, Type};
use heklang::value::{self, Defs};
use heklang::{Entry, Event, Json, Kind, Program, Record, Value};
use tephra::{Position, QueryItem, Tag, Tags};

use crate::context::CommandContext;
use crate::crypto::KeyStore;
use crate::envelope;
use crate::hash::sha256_hex;
use crate::http::{HttpClient, HttpRequest};
use crate::metrics;
use crate::opdb::OpDb;
use crate::read_api;
use crate::read_model::ReadModel;
use crate::schema::{self, EmittedEvent, EventDef, EventDefs, FieldKind};
use crate::secrets::SecretStore;
use crate::store::Store;

/// heklang counts positions from zero and tephra counts from one, so the two are one
/// apart everywhere. Written once here rather than remembered at each call site.
///
/// The heads agree: a log holding tephra positions `1..=n` is heklang positions
/// `0..=n-1`, and both call the next append `n`. That is why an append condition
/// crosses unchanged while a record's position does not.
pub(crate) fn to_tephra(position: u64) -> Position {
    Position::new(position + 1)
}

pub(crate) fn from_tephra(position: Position) -> u64 {
    position.get().saturating_sub(1)
}

fn host_error(err: impl std::fmt::Display) -> Error {
    Error::new(ErrorKind::Host(err.to_string()))
}

// ---------------------------------------------------------------------------
// JSON, both ways
// ---------------------------------------------------------------------------

/// `serde_json` is what the envelope and the network speak; `heklang::Json` is what
/// rule 8's table is written against.
pub fn to_heklang_json(value: &serde_json::Value) -> Json {
    match value {
        serde_json::Value::Null => Json::Null,
        serde_json::Value::Bool(flag) => Json::Bool(*flag),
        // Its own text, whole or not. `Json::Num` holds what the wire said, so a
        // foreign body is handed on unrounded and unreformatted rather than being
        // squeezed through an `i64` and losing everything that did not fit.
        serde_json::Value::Number(number) => Json::num(number.to_string()),
        serde_json::Value::String(text) => Json::Str(text.clone()),
        serde_json::Value::Array(items) => Json::Arr(items.iter().map(to_heklang_json).collect()),
        serde_json::Value::Object(fields) => Json::Obj(
            fields
                .iter()
                .map(|(name, value)| (name.clone(), to_heklang_json(value)))
                .collect(),
        ),
    }
}

pub fn from_heklang_json(value: &Json) -> serde_json::Value {
    match value {
        Json::Null => serde_json::Value::Null,
        Json::Bool(flag) => serde_json::Value::Bool(*flag),
        // Back through serde as a number, not as its text. The fallback is unreachable
        // from hekla, which only ever builds a `Num` out of one of these, and keeping
        // the bytes beats inventing a value for something that cannot arrive.
        Json::Num(text) => text.parse::<serde_json::Number>().map_or_else(
            |_| serde_json::Value::String(text.clone()),
            serde_json::Value::Number,
        ),
        Json::Str(text) => serde_json::Value::String(text.clone()),
        // The **redaction**, never the credential. This function feeds the event log and
        // the read models, which are the two places a credential must never reach, and
        // an arm that spelled it out would put one there permanently the first time an
        // unreachable case stopped being unreachable. Unreachable today because
        // `Json::wire` and `Json::shown` both strip the variant before a host is handed
        // a request, and because rule 16 keeps a `Secret` out of an `emit` and a
        // projector write; this is what that costs if either ever stops being true.
        Json::Secret { redacted, .. } => serde_json::Value::String(redacted.clone()),
        Json::Arr(items) => serde_json::Value::Array(items.iter().map(from_heklang_json).collect()),
        Json::Obj(fields) => serde_json::Value::Object(
            fields
                .iter()
                .map(|(name, value)| (name.clone(), from_heklang_json(value)))
                .collect(),
        ),
    }
}

/// The tag text of a filter value. Rule 8's table is the single answer for how a value
/// looks when it leaves the process, so a tag written at append and a tag matched at
/// read cannot disagree about it.
fn tag_text(value: &Value) -> String {
    value::text(value)
}

// ---------------------------------------------------------------------------
// The host
// ---------------------------------------------------------------------------

/// heklang's harness epoch, `2020-01-01T00:00:00Z`, and the step it advances by per
/// record. Both are private to `heklang::Harness`, so these are copies, and
/// `a_pinned_envelope_is_the_one_heklangs_own_harness_writes` is what keeps them copies
/// rather than guesses.
const PINNED_EPOCH_MICROS: i64 = 1_577_836_800_000_000;
const PINNED_STEP_MICROS: i64 = 60_000_000;

/// Where an appended envelope's id and its timestamp come from.
///
/// One field rather than two, because the two halves have to move together. A
/// positional timestamp beside a counter-minted id is a world that agrees with
/// `hek test` about when an event happened and disagrees about which event it was, and
/// that is not a state worth being able to spell.
pub enum Stamp {
    /// A live append: a fresh v4 id, and the wall clock read once per request and
    /// pinned across every attempt (heklang's rule 11). RFC 3339.
    Wall(String),
    /// `heklang::Harness`, reproduced. Both halves derive from the log position, so one
    /// `test` declaration synthesises the same envelope under `hek test` and
    /// `hekla test`.
    Pinned,
}

/// The instant heklang's harness stamps the record at `position`.
///
/// **A pinned world holds about 4.2e9 events**, and the three functions here stop in
/// that order. A minute a record puts `time`'s year-9999 ceiling first, at roughly
/// `4.2e9`; the multiply below overflows `i64` at about `1.5e11`; the id's twelve-digit
/// field runs out at `1e12`. So the first limit reached is the renderable date range,
/// and every one of them is unreachable by a log that has to be built one `given` at a
/// time. That is the licence the two panics below run on.
fn pinned_at(position: u64) -> i64 {
    PINNED_EPOCH_MICROS + position as i64 * PINNED_STEP_MICROS
}

/// The same instant as RFC 3339, which is what an envelope holds.
///
/// Panics rather than falling back, because the only fallback worth writing is an
/// epoch, and a stamp that silently reads `1970-01-01T00:00:00Z` is precisely the
/// failure this whole arm exists to remove.
fn pinned_stamp(position: u64) -> String {
    rfc3339(pinned_at(position)).expect("a harness position renders inside the date range")
}

/// The id heklang's harness gives the record at `position`.
///
/// **The counter is decimal, in a field that is read as hex.** heklang writes
/// `format!("...-{position:012}")`, so position 10 is `…-000000000010` and not
/// `…-00000000000a`. Deriving the id arithmetically from a `u128` looks equivalent and
/// diverges at the tenth event, which is deep enough into a test to be found from the
/// wrong end. Formatting and parsing back is what keeps the two byte for byte.
///
/// The parse cannot fail on the digits, since a decimal digit is always a hex digit. It
/// can fail on the *length*, once `position` needs more than the twelve the field
/// holds, which is the `1e12` named on [`pinned_at`] and the last of the three limits
/// to be reached.
fn pinned_id(position: u64) -> uuid::Uuid {
    uuid::Uuid::parse_str(&format!("0190d1a1-0000-7000-9000-{position:012}"))
        .expect("a harness position fits the id's twelve digits")
}

/// One request's or one invocation's world.
///
/// Built per run rather than shared: it carries the causation metadata and the pinned
/// append time of the thing being run, which is exactly the scope tephra's writer is
/// not.
pub struct HeklaHost {
    pub program: Arc<Program>,
    pub events: Arc<EventDefs>,
    pub store: Store,
    pub keystore: Option<Arc<KeyStore>>,
    /// Causation for the events this run appends. heklang has no opinion on it, which
    /// is why it stays hekla's.
    pub ctx: CommandContext,
    /// Where the envelope this run appends gets its id and its append time. heklang
    /// pins its own `now()` per invocation through [`Clock`]; this is what answers it.
    pub stamp: Stamp,
    pub idem_tag: Option<String>,
    /// The journal identity of the call being made right now, shared with this
    /// invocation's [`Journal`].
    ///
    /// An `invoke` appends through this host, and the append needs an idempotency tag
    /// that is the same on every replay of that call. Only the journal knows it: the
    /// language hands `Calls::recorded` the call key and its ordinal immediately before
    /// it runs the command, and hands this host nothing. So the journal writes it here
    /// and [`Log::append`] reads it back, which is the one piece of information the two
    /// traits have to share.
    pub call: Option<Arc<Mutex<Option<String>>>>,
    /// Where the last successful append landed, for the caller to report.
    pub appended: Option<tephra::PositionRange>,
    /// What was appended, in the stored form a response reports. Built on the way
    /// through `lower` rather than reconstructed after, so the tags a caller reports
    /// are the tags the log actually carries.
    pub emitted: Vec<EmittedEvent>,
    /// Set when the store refused the append for a transient reason. Carried out of
    /// band because heklang has one error for "the host could not", while a draining
    /// writer is a retryable status rather than a failure.
    pub unavailable: Option<String>,
    /// Set when the append was refused because this request already committed under
    /// its idempotency key. Carried out of band for the same reason `unavailable` is:
    /// the language has one error for "the host could not", and this is not a failure.
    pub duplicated: bool,
    /// The network, absent for a command: heklang's parser guarantees a command never
    /// reaches `http.*`, so a command's world has nothing to give it.
    pub http: Option<Arc<dyn HttpClient>>,
    /// What this deployment supplies for the project's `secret` declarations, absent for
    /// the same reason `http` is: rule 16 gates a read to an effect arm and an
    /// effect-local `fn`, so a command's world has no credential to give either. Keeping
    /// it `None` there makes that a structural guarantee rather than a convention.
    ///
    /// Resolved once per process and shared, unlike everything else on this struct,
    /// which is per run. A **sealed replay still gets the real one**: a read is
    /// unjournaled, so it re-runs the way `reveal` does, and a replay answering nothing
    /// would wedge on `MissingSecret` and report a divergence that is not there.
    pub secrets: Option<Arc<SecretStore>>,
    /// The window a rate limiter asked for, if a retryable response named one in
    /// seconds. Written here rather than handed to the language: heklang decides
    /// *whether* to retry and this decides what one attempt costs, and only a host has
    /// a clock to wait on. The driver reads it back off the host after the invocation.
    pub retry_after: Option<Duration>,
    /// Why the last attempt did not reach the far side. Rule 5 makes the attempts
    /// invisible to the program, so the language reports only that the URL did not
    /// answer; an operator needs the reason, and this is the only place that still has
    /// it.
    pub last_transport: Option<String>,
    /// A replay that audits rather than acts: [`Log::append`] and [`Keys::erase`]
    /// refuse, the way a sealed replay's HTTP client refuses a send.
    ///
    /// Blocking the transport is not enough on its own, because heklang performs a
    /// journal miss for real: `invoke` runs the target command and appends, and `erase`
    /// destroys the key. Both reach the store through this host, so this is where they
    /// have to stop. A check that can cause the fault it looks for is worse than no
    /// check.
    pub sealed: bool,
}

impl HeklaHost {
    /// One stored event as heklang reads it.
    fn record_of(&self, position: Position, event: tephra::EventRef<'_>) -> Result<Record, Error> {
        record_of(&self.program, position, event)
    }
}

/// One stored event as heklang reads it: every field typed by its declaration, a
/// subject-scoped one still sealed, and one the payload predates read as whatever its
/// declaration says absence means.
///
/// Free rather than a method because a projector thread reads the log without a
/// [`HeklaHost`]: it has no clock, no network and nothing to append. **It no longer
/// needs a key store either**, which is what closing the ciphertext gap bought: reading
/// the log is not a place key material has to reach.
pub fn record_of(
    program: &Program,
    position: Position,
    event: tephra::EventRef<'_>,
) -> Result<Record, Error> {
    let ty = event.event_type();
    let path = EventPath::new(ty.split('.'));
    let declared = program
        .event(&path)
        .ok_or_else(|| host_error(format!("event type `{ty}` is not declared")))?;
    let (envelope, data) = envelope::decode(event.data()).map_err(host_error)?;

    let defs = Defs::of(program);
    let mut fields = BTreeMap::new();
    for field in &declared.fields {
        // **Nothing is decrypted here**, and that is the whole of what this read costs.
        // A subject-scoped field crosses as the ciphertext it is stored as; heklang
        // seals it as it binds it, and `Keys::decrypt` opens it at the one `reveal` that
        // asks. Decrypting on the way in instead cost 3.5µs a record and made a fold
        // four times its own cost, for content a fold does not read (`tests/measure.rs`).
        //
        // It also deletes a placeholder. When this decrypted eagerly, a shredded key
        // left nothing to put in a present field, and rule 12 needs absent and erased to
        // stay different rows. Carrying the ciphertext means there is always something
        // to carry and nothing to stand in for it.
        //
        // The `Option` is the whole of the second reading. A key that is there holding
        // `null` is a value a producer wrote; a key that is not there is a field this
        // payload predates, which the declaration may answer with `@absent`. Passing
        // `Json::Null` for both, as this did, collapsed them and made every `@absent`
        // unreachable from the one place that reads history.
        let stored = data.get(&field.name).map(to_heklang_json);
        let read = value::stored_field(field, stored.as_ref(), defs)
            .map_err(|why| Error::new(ErrorKind::Mismatch(why)))?;
        fields.insert(field.name.clone(), read);
    }

    let at = value::timestamp(&envelope.timestamp).ok_or_else(|| {
        host_error(format!(
            "envelope timestamp `{}` is not RFC 3339",
            envelope.timestamp
        ))
    })?;
    Ok(Record::new(
        envelope.event_id.to_string(),
        from_tephra(position),
        at,
        Event { path, fields },
    ))
}

/// Why a declared event type's history cannot be read.
#[derive(Debug)]
pub enum Fault {
    /// A field this program declares that a recorded declaration of the same shape did
    /// not, for an event type the log holds. No payload written under that declaration
    /// can carry the field, because an `emit` writes an event whole.
    ///
    /// **Complete**, unlike [`Fault::Undecodable`]: it is settled from the declaration
    /// table and one existence read rather than from a sampled event, so which events
    /// the log happens to hold cannot hide it.
    Unanswered {
        /// The dotted path, `note` or `detail.weight`.
        path: String,
        /// Sealed content has no plaintext literal, so heklang refuses `@absent` on it
        /// and being optional is the only repair there is.
        sealed: bool,
    },
    /// A stored event this program could not decode. One event per type, so this catches
    /// what it samples and promises nothing about the rest: see [`unreadable_history`].
    Undecodable { position: u64, detail: Error },
}

/// One declared event type whose history this deployment cannot read, and why.
#[derive(Debug)]
pub struct Unreadable {
    /// The event type as the store holds it, with no leading `@`.
    pub event_type: String,
    pub fault: Fault,
}

/// The repair a fault calls for. Grouped rather than repeated per event, because ten
/// events failing the same way call for one instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Advice {
    /// A field younger than the log, which a declaration can answer for itself.
    Absent,
    /// The same, sealed, where the annotation is not available.
    SealedOptional,
    /// A value that never fitted, which no annotation answers.
    Retype,
    /// A payload hekla could not read at all, which no declaration edit repairs.
    Corrupt,
}

impl Unreadable {
    /// A tephra position when an event was actually read, which is what `/admin/events`
    /// lists. `None` when the declaration table settled it and no event was opened.
    pub fn position(&self) -> Option<u64> {
        match &self.fault {
            Fault::Unanswered { .. } => None,
            Fault::Undecodable { position, .. } => Some(*position),
        }
    }

    /// Whether the stored payload has no key for the field, rather than a key holding
    /// something that no longer fits.
    ///
    /// The one fault a declaration can answer for itself, so it is the one that gets
    /// told to add `@absent`. Offering an annotation that cannot help would send an
    /// author the wrong way.
    pub fn is_absence(&self) -> bool {
        matches!(self.advice(), Advice::Absent | Advice::SealedOptional)
    }

    /// What could not be read, in one clause.
    pub fn reason(&self) -> String {
        match &self.fault {
            Fault::Unanswered { path, .. } => {
                format!("no stored event can carry `{path}`")
            }
            Fault::Undecodable { detail, .. } => match &detail.kind {
                ErrorKind::Mismatch(why) if why.is_absence() => {
                    format!("the stored payload has no `{}`", why.path.join("."))
                }
                _ => detail.to_string(),
            },
        }
    }

    fn advice(&self) -> Advice {
        match &self.fault {
            Fault::Unanswered { sealed: true, .. } => Advice::SealedOptional,
            Fault::Unanswered { .. } => Advice::Absent,
            Fault::Undecodable { detail, .. } => match &detail.kind {
                ErrorKind::Mismatch(why) if !why.is_absence() => Advice::Retype,
                ErrorKind::Mismatch(why) if matches!(why.expected, Type::Sealed(..)) => {
                    Advice::SealedOptional
                }
                ErrorKind::Mismatch(_) => Advice::Absent,
                // `record_of` also fails on a payload `envelope::decode` cannot parse and
                // on a timestamp that is not RFC 3339. Neither is a declaration that
                // outran its log, and neither is repaired by editing one.
                _ => Advice::Corrupt,
            },
        }
    }
}

/// What to do about a set of unreadable events, one sentence per distinct repair.
///
/// Shared between the boot refusal and `hekla plan`, so the two cannot come to say
/// different things about the same log.
pub fn unreadable_guidance(unreadable: &[Unreadable]) -> Vec<&'static str> {
    let mut wanted: Vec<Advice> = unreadable.iter().map(Unreadable::advice).collect();
    wanted.sort_unstable();
    wanted.dedup();
    wanted
        .into_iter()
        .map(|advice| match advice {
            Advice::Absent => {
                "A field was added to an event that already has instances, and no payload written before it can carry one: say what it reads as with `@absent(<value>)`, or make it optional so the type itself says absence is possible."
            }
            Advice::SealedOptional => {
                "A subject-scoped field cannot take `@absent`, because sealed content has no plaintext literal to give: make it optional, which is what an erased subject's value needs anyway."
            }
            Advice::Retype => {
                "A stored value that no longer fits its field cannot be answered by `@absent`, because the field is there: a field's type is part of the fact, so declare the new shape under a new name carrying `@absent` and drop the old field, which stops being decoded once nothing declares it."
            }
            Advice::Corrupt => {
                "One stored event could not be read at all, which is a payload rather than a declaration: no edit to a `.hk` file repairs it, and `hekla verify` against a copy of the directory is where to look next."
            }
        })
        .collect()
}

/// Every field this program declares that a recorded declaration of the same shape did
/// not, for an event type the log actually holds.
///
/// **This is the complete half of the check.** An `emit` writes an event whole, so a
/// field a declaration did not have cannot be in any payload written under it; and every
/// deploy records its declarations, so every shape the log was written under is on hand
/// here. What the log holds then needs one existence read per affected type, which is a
/// question about the type and not about any particular event.
///
/// It answers nothing about a value that no longer fits its field. That is
/// [`unreadable_history`]'s half, and that one is a sample.
///
/// A record is reported at the path an event carries it (`detail.weight`), because a
/// record has no history of its own: it is only ever reached from an event.
pub fn unanswered_history(
    program: &Program,
    recorded: &[Entry],
    store: &Store,
) -> anyhow::Result<Vec<Unreadable>> {
    let younger = fields_younger_than_the_log(program, recorded);
    if younger.is_empty() {
        return Ok(Vec::new());
    }
    let records: BTreeMap<&str, &RecordDef> = program
        .records
        .iter()
        .map(|def| (def.name.as_str(), def))
        .collect();

    let mut found = Vec::new();
    for declared in &program.events {
        let path = declared.path.to_string();
        let mut paths: Vec<(String, bool)> = Vec::new();
        for field in &declared.fields {
            if !field.answers_absence()
                && younger
                    .get(&(Kind::Event, path.clone()))
                    .is_some_and(|names| names.contains(&field.name))
            {
                paths.push((field.name.clone(), matches!(field.ty, Type::Sealed(..))));
            }
            let mut reached = Vec::new();
            records_under(
                &field.ty,
                &field.name,
                &records,
                &mut Vec::new(),
                &mut reached,
            );
            for (prefix, name) in reached {
                let Some(def) = records.get(name.as_str()) else {
                    continue;
                };
                for nested in &def.fields {
                    if !nested.answers_absence()
                        && younger
                            .get(&(Kind::Record, name.clone()))
                            .is_some_and(|names| names.contains(&nested.name))
                    {
                        paths.push((format!("{prefix}.{}", nested.name), false));
                    }
                }
            }
        }
        if paths.is_empty() {
            continue;
        }
        // One question per type, asked once however many fields it owes for: has
        // anything of this type ever been written? Whatever it was, it was written
        // under a declaration that lacked these.
        if !holds_any(store, &declared.path.segments.join("."))? {
            continue;
        }
        paths.sort();
        paths.dedup();
        for (path, sealed) in paths {
            found.push(Unreadable {
                event_type: declared.path.segments.join("."),
                fault: Fault::Unanswered { path, sealed },
            });
        }
    }
    Ok(found)
}

/// Every declared event type whose oldest stored event this program cannot read.
///
/// **A sample, and deliberately one.** A complete answer means decoding every event in
/// the log, which is work proportional to history at every boot. One event per type is
/// bounded and catches the drift that is wrong on every event of its type; it does not
/// catch drift that is wrong on only some of them, and a narrowed enum whose oldest
/// event happens to hold a surviving variant is the case to have in mind.
///
/// Decoded with [`record_of`] itself rather than a walk of its own, so what it does
/// catch cannot be a false alarm: a failure here **is** a fold, a projector or an effect
/// lane failing, one event early.
///
/// The complete half of the check is [`unanswered_history`], which needs no event.
pub fn unreadable_history(program: &Program, store: &Store) -> anyhow::Result<Vec<Unreadable>> {
    let mut found = Vec::new();
    for declared in &program.events {
        let event_type = declared.path.segments.join(".");
        let query = query_of_types(slice::from_ref(&event_type)).map_err(|err| anyhow!("{err}"))?;
        // The cap reaches tephra's planning, so a type with no events costs an index
        // probe rather than a scan.
        let mut reads = store.read(&query, Position::ZERO, Some(1));
        let Some(item) = reads.next() else { continue };
        let seq = item.map_err(|err| anyhow!("reading the event log: {err}"))?;
        let position = seq.position.get();
        if let Err(detail) = record_of(program, seq.position, seq.event) {
            found.push(Unreadable {
                event_type,
                fault: Fault::Undecodable { position, detail },
            });
        }
    }
    Ok(found)
}

/// Both halves, with the sampled one silent about a type the complete one has already
/// answered for: the same event would only be reported twice, once with worse advice.
pub fn history_faults(
    program: &Program,
    recorded: &[Entry],
    store: &Store,
) -> anyhow::Result<Vec<Unreadable>> {
    let mut found = unanswered_history(program, recorded, store)?;
    let answered: BTreeSet<&str> = found.iter().map(|one| one.event_type.as_str()).collect();
    let sampled: Vec<Unreadable> = unreadable_history(program, store)?
        .into_iter()
        .filter(|one| !answered.contains(one.event_type.as_str()))
        .collect();
    found.extend(sampled);
    Ok(found)
}

/// Whether the log holds any event of this type at all.
fn holds_any(store: &Store, event_type: &str) -> anyhow::Result<bool> {
    let query =
        query_of_types(slice::from_ref(&event_type.to_owned())).map_err(|err| anyhow!("{err}"))?;
    let mut reads = store.read(&query, Position::ZERO, Some(1));
    match reads.next() {
        Some(item) => {
            item.map_err(|err| anyhow!("reading the event log: {err}"))?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// The field names this program declares that some recorded version of the same
/// declaration did not, keyed by kind and the name the digest knows it by.
///
/// Every recorded version, not only the current one: a field removed in one deploy and
/// put back in the next leaves events in the middle that carry neither, and comparing
/// against the current declaration alone would see the two ends agree and miss them.
fn fields_younger_than_the_log(
    program: &Program,
    recorded: &[Entry],
) -> BTreeMap<(Kind, String), BTreeSet<String>> {
    let mut candidate: BTreeMap<(Kind, String), BTreeSet<String>> = BTreeMap::new();
    for def in &program.events {
        candidate.insert(
            (Kind::Event, def.path.to_string()),
            def.fields.iter().map(|field| field.name.clone()).collect(),
        );
    }
    for def in &program.records {
        candidate.insert(
            (Kind::Record, def.name.clone()),
            def.fields.iter().map(|field| field.name.clone()).collect(),
        );
    }

    let mut younger: BTreeMap<(Kind, String), BTreeSet<String>> = BTreeMap::new();
    for entry in recorded {
        let key = (entry.kind, entry.name.clone());
        let Some(now) = candidate.get(&key) else {
            continue;
        };
        let then: BTreeSet<&str> = entry.field_names().into_iter().collect();
        let added = now.iter().filter(|name| !then.contains(name.as_str()));
        younger.entry(key).or_default().extend(added.cloned());
    }
    younger.retain(|_, names| !names.is_empty());
    younger
}

/// Every `(path, record name)` a declared field reaches, so a field added to a record is
/// reported where an event actually carries it.
fn records_under(
    ty: &Type,
    prefix: &str,
    records: &BTreeMap<&str, &RecordDef>,
    stack: &mut Vec<String>,
    out: &mut Vec<(String, String)>,
) {
    match ty {
        // A container does not add a path segment: which element of a list carries the
        // field is a fact about one payload, and this is a question about every one.
        Type::Opt(inner) | Type::List(inner) | Type::Sealed(inner, _) => {
            records_under(inner, prefix, records, stack, out);
        }
        Type::Map(_, value) => records_under(value, prefix, records, stack, out),
        Type::Record(name) => {
            if stack.iter().any(|seen| seen == name) {
                return;
            }
            out.push((prefix.to_owned(), name.clone()));
            let Some(def) = records.get(name.as_str()) else {
                return;
            };
            stack.push(name.clone());
            for field in &def.fields {
                let nested = format!("{prefix}.{}", field.name);
                records_under(&field.ty, &nested, records, stack, out);
            }
            stack.pop();
        }
        _ => {}
    }
}

/// The message a runtime refuses to start with, or `None` when it can read its history.
///
/// Every unreadable type at once rather than the first, matching [`crate::secrets`]: an
/// operator fixing one declaration per restart is the failure mode that shape exists to
/// avoid. `repair` completes "…, so `<repair>`", because what there is to do about it is
/// the caller's to know: `serve` has written nothing yet, and a sweep was never going to
/// write anything.
pub fn unreadable_refusal(unreadable: &[Unreadable], verb: &str, repair: &str) -> Option<String> {
    if unreadable.is_empty() {
        return None;
    }
    let named = unreadable
        .iter()
        .map(|one| match one.position() {
            Some(position) => format!(
                "`@{}` at position {position} ({})",
                one.event_type,
                one.reason()
            ),
            None => format!("`@{}` ({})", one.event_type, one.reason()),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let guidance = unreadable_guidance(unreadable).join(" ");
    Some(format!(
        "this program cannot read events that are already in the log, so it cannot {verb}: {named}, so {repair}. {guidance}"
    ))
}

impl HeklaHost {
    /// The tag this append is guarded against, if anything guards it.
    ///
    /// A command run for a request carries the caller's key. A command run by an
    /// effect's `invoke` carries the journal identity of that call instead, which is
    /// the same on every replay and different for every call, so a crash between the
    /// append and the journal write replays into the existence clause rather than
    /// appending a second time.
    fn idempotency_tag(&self) -> Option<String> {
        if let Some(tag) = &self.idem_tag {
            return Some(tag.clone());
        }
        let call = self.call.as_ref()?;
        let held = call.lock().ok()?;
        held.clone()
    }

    /// One resolved slice as a tephra query item.
    ///
    /// A filter is never on a subject-scoped field: heklang rejects an equality on
    /// sealed content at parse time (rule 12), so every value here is plaintext and
    /// matches the tag verbatim.
    fn item_of(&self, slice: &Predicate) -> Result<QueryItem, Error> {
        let ty = schema::event_type(&slice.event);
        let event_type = tephra::EventType::new(ty.as_str()).map_err(host_error)?;
        let mut tags = Vec::new();
        for (field, value) in &slice.filters {
            let raw = format!("{field}:{}", tag_text(value));
            tags.push(Tag::new(raw).map_err(host_error)?);
        }
        Ok(QueryItem::new(
            vec![event_type],
            Tags::new(tags).map_err(host_error)?,
        ))
    }

    /// The handle this host appends through, or the error for a log it may only read.
    ///
    /// A follower cannot extend the log, and refusing here rather than assuming is what
    /// keeps `hekla plan --replay` structurally incapable of writing to the deployment
    /// it is reporting on.
    fn writer(&self) -> Result<&tephra::WriteHandle, Error> {
        self.store
            .writer("a handler")
            .map_err(|err| host_error(err.to_string()))
    }

    fn query_of(&self, slices: &[Predicate]) -> Result<tephra::Query, Error> {
        let items = slices
            .iter()
            .map(|slice| self.item_of(slice))
            .collect::<Result<Vec<_>, Error>>()?;
        Ok(tephra::Query::items(items))
    }
}

impl Log for HeklaHost {
    fn head(&self) -> Result<u64, Error> {
        Ok(self.store.head().get())
    }

    fn record(&self, position: u64) -> Result<Option<Record>, Error> {
        let at = to_tephra(position);
        let mut reads =
            self.store
                .read(&tephra::Query::all(), Position::new(at.get() - 1), Some(1));
        match reads.next() {
            Some(item) => {
                let seq = item.map_err(host_error)?;
                if seq.position != at {
                    return Ok(None);
                }
                self.record_of(seq.position, seq.event).map(Some)
            }
            None => Ok(None),
        }
    }

    fn read(
        &self,
        query: &Query,
        visit: &mut dyn FnMut(&Record) -> Result<(), Error>,
    ) -> Result<(), Error> {
        if query.slices.is_empty() {
            return Ok(());
        }
        let lowered = self.query_of(&query.slices)?;
        let last = query.upto.map(to_tephra);
        // tephra's `after` is an exclusive lower bound and its positions are one higher
        // than heklang's, so an inclusive heklang `from` crosses as itself: `from = 0` is
        // `Position::ZERO` and reads the whole log. It is pushed into planning rather than
        // filtered afterwards, which is what makes a retry's delta read cost the delta.
        let mut reads = self.store.read(&lowered, Position::new(query.from), None);
        while let Some(item) = reads.next() {
            let seq = item.map_err(host_error)?;
            // Rule 3: an effect's fold stops at its own trigger, inclusive.
            if last.is_some_and(|limit| seq.position > limit) {
                break;
            }
            let record = self.record_of(seq.position, seq.event)?;
            visit(&record)?;
        }
        Ok(())
    }

    fn append(&mut self, events: &[Event], condition: &AppendCondition) -> Result<(), Error> {
        if self.sealed {
            return Err(host_error("a sealed replay tried to append to the log"));
        }
        // Asked for before anything is lowered, not at the append itself: refusing
        // halfway through sealing would have spent key material on a request that wrote
        // nothing. Nothing reaches this today, because only a sealed replay follows and
        // the line above returns first, but a check must never be able to cause the
        // fault it looks for, and that is a claim about what the code makes impossible
        // rather than about what it happens to reach.
        self.writer()?;
        // A command that decided to do nothing still commits, and heklang appends its
        // (empty) outcome rather than special-casing it. There is nothing to write and
        // nothing a condition could guard, so this is where that stops.
        if events.is_empty() {
            return Ok(());
        }
        // Where these events land, which is what a pinned envelope is derived from.
        // Read per attempt rather than carried on the host: an attempt that loses the
        // race lowers the same events again from a head that has moved, so the stamps a
        // run assigns are independent of how many times it was beaten to the log.
        //
        // Across attempts, not within one. The envelope is sealed here and the append
        // happens below, so this is only the landing position if nothing commits in
        // between; the arm at the bottom is where that stops being an assumption.
        let base = self.head()?;
        let mut built = Vec::with_capacity(events.len());
        let mut emitted = Vec::with_capacity(events.len());
        for (offset, event) in events.iter().enumerate() {
            let (stored, reported) = self.lower(event, base + offset as u64)?;
            built.push(stored);
            emitted.push(reported);
        }
        // Two clauses, with separate positions: the moving decision boundary heklang
        // resolved, and the whole-log uniqueness of this request. A duplicate that
        // committed anywhere is caught even once `after` has advanced past it, which
        // is what makes a keyed command exactly-once against the log itself rather
        // than against op-DB bookkeeping.
        //
        // The idempotency clause is hekla's alone. heklang has no idea the request has
        // a key, which is why it is added here and not in `condition.slices`.
        let boundary = tephra::AppendCondition::new(self.query_of(&condition.slices)?)
            .after(Position::new(condition.after));
        let dcb = match self.idempotency_tag() {
            Some(tag) => boundary.fail_if_exists(tephra::Query::item(idem_item(&tag)?)),
            None => boundary,
        };
        let landed = self.writer()?.append(built, Some(dcb));
        match landed {
            Ok(range) => {
                // A pinned envelope was sealed for the position it was lowered at, so a
                // batch landing anywhere else carries an id and an append time
                // belonging to some other record: wrong in the log, wrong forever, and
                // silent, because nothing downstream re-derives either from where the
                // event actually sits. One writer ever holds a pinned world (a `hekla
                // test` case owns its store and heklang runs cases in sequence), which
                // is what makes `base` the landing position rather than merely the
                // likely one. This is that claim, checked rather than assumed.
                if matches!(self.stamp, Stamp::Pinned) && from_tephra(range.first) != base {
                    return Err(host_error(format!(
                        "a pinned append lowered for position {base} landed at {}; \
                         something else is writing to this world's log",
                        from_tephra(range.first)
                    )));
                }
                self.appended = Some(range);
                self.emitted = emitted;
                Ok(())
            }
            // A draining writer is not a failed program. It leaves through the same
            // error the language has for a host that could not, and the field beside
            // it is what tells the caller to answer retryable rather than broken.
            Err(tephra::AppendError::Shutdown) => {
                self.unavailable =
                    Some("the write coordinator is shutting down; retry".to_string());
                Err(host_error("the write coordinator is shutting down; retry"))
            }
            // A duplicate of this very request already committed, caught atomically at
            // the append. Not a conflict to retry: the work is done, and the caller
            // recovers the original outcome rather than re-deciding.
            Err(tephra::AppendError::Conflict {
                clause: tephra::ConflictClause::Existence,
                ..
            }) => {
                self.duplicated = true;
                Err(host_error("this request already committed"))
            }
            // Being beaten to the log inside the boundary is not a host failure: it is
            // the one answer the language has a shape for, and the attempt loop inside
            // `run_retrying` is what reads it.
            Err(tephra::AppendError::Conflict { .. }) => Err(Error::new(ErrorKind::Conflict {
                after: condition.after,
            })),
            Err(err) => Err(host_error(err)),
        }
    }
}

/// Rule 16's other half. A world with no store answers nothing, which is a command's
/// world and the bare appender a test seeds a log through; neither can reach a `secret`,
/// so neither is ever asked. Unjournaled by design, so this is called once per read on
/// every attempt and every replay, and a rotation therefore takes effect on the next
/// restart with no recorded answer left to invalidate.
impl Secrets for HeklaHost {
    fn secret(&self, name: &str) -> Option<Arc<str>> {
        self.secrets.as_ref()?.get(name)
    }
}

impl Clock for HeklaHost {
    fn now(&self) -> i64 {
        match &self.stamp {
            Stamp::Wall(text) => value::timestamp(text).unwrap_or(0),
            // Through [`Log::head`] rather than the store directly, so the
            // tephra-to-heklang position convention stays in the one place that states
            // it. Spelling it again here would put `now()` a minute off the position
            // the next append is stamped at the day that convention moves, which is the
            // exact divergence this arm exists to remove. Infallible: `Log::head` for
            // this host is a load wrapped in `Ok`, and heklang lowers `now()` into one
            // slot per invocation, so it is asked once and asked cheaply.
            Stamp::Pinned => pinned_at(Log::head(self).expect("a store head is a load")),
        }
    }
}

impl Keys for HeklaHost {
    /// The one place a subject key is read, and it is reached once per `reveal` rather
    /// than once per record: heklang carries the ciphertext and asks only for what a
    /// handler actually reveals.
    ///
    /// `field` is the name the content was sealed under, which [`KeyStore`] binds into
    /// the ciphertext, so content that was moved decrypts under the name it was sealed
    /// with rather than under wherever it now sits.
    fn decrypt(
        &self,
        subject: &str,
        id: &str,
        field: &str,
        content: &str,
    ) -> Result<Option<String>, Error> {
        let Some(keystore) = self.keystore.as_deref() else {
            return Err(host_error(format!(
                "field `{field}` is scoped to `{subject}` but no master key is configured"
            )));
        };
        keystore
            .decrypt_subject(subject, id, field, content)
            .map_err(host_error)
    }

    fn erase(&mut self, subject: &str, id: &str) -> Result<(), Error> {
        if self.sealed {
            return Err(host_error("a sealed replay tried to erase a subject key"));
        }
        let Some(keystore) = self.keystore.as_deref() else {
            return Err(host_error(
                "erase needs a master key, but none is configured",
            ));
        };
        keystore.erase(subject, id).map_err(host_error)?;
        Ok(())
    }
}

/// A status the language re-sends on rather than handing to a handler: each names a
/// condition that clears on its own, with the same request. Mirrors heklang's own
/// `is_retryable`, because this only reads a header the language never sees.
fn is_retryable(status: u16) -> bool {
    matches!(status, 408 | 425 | 429) || status >= 500
}

/// The `Retry-After` a retryable response asked for, if it named one in seconds.
///
/// The header's other legal form is an HTTP-date (RFC 9110 10.2.3). Honoring that
/// would mean taking on a date parser and turning the peer's clock into a duration
/// against ours, so a date reads as absent and the wedge backoff applies unchanged.
fn retry_after_hint(headers: &[(String, String)]) -> Option<Duration> {
    let value = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        .map(|(_, value)| value.trim())?;
    // Rejects a date, a negative, and anything else non-numeric, all as "absent".
    value.parse::<u64>().ok().map(Duration::from_secs)
}

/// The wire method behind a builtin's name. heklang identifies a call by the builtin
/// that made it (`http.post`), which is what its journal key and its diagnostics say;
/// a request on the network says `POST`.
fn http_method(verb: &str) -> String {
    verb.rsplit('.').next().unwrap_or(verb).to_uppercase()
}

impl HeklaHost {
    /// The one leak rule 16's redaction cannot reach on its own.
    ///
    /// heklang renders `ErrorKind::Unreachable` from `Request::shown`, so the language's
    /// own message is safe. This string is not the language's: it is the transport's,
    /// written *below* the seam by `ureq` from whatever it chose to include. The wedge
    /// message concatenates the two (`effect::try_invocation`), and from there it reaches
    /// `/status`, `/admin` and `tracing`. A `secret DISCORD_WEBHOOK` used as a url is a
    /// credential that is entirely a url, so anything echoing it publishes the whole
    /// thing.
    ///
    /// Two passes, because one is not enough. Substituting the url handles the case where
    /// the transport echoed it verbatim, and is exact. But `ureq` is free to render a
    /// *parsed* uri instead: a default port dropped, an empty path filled in, a reserved
    /// character percent-encoded, and the substitution silently misses. So the store's own
    /// scrubber runs after it, scanning for the credential values themselves, which
    /// survives any framing the transport put around them. Measured rather than assumed:
    /// `ureq` 3 renders a DNS failure with no url at all, which is why the first pass on
    /// its own could not be shown to be doing anything.
    fn redact_transport(&self, reason: &str, request: &Request) -> String {
        let substituted = if request.wire.url == request.shown.url {
            reason.to_owned()
        } else {
            reason.replace(&request.wire.url, &request.shown.url)
        };
        match &self.secrets {
            Some(store) => store.redact(&substituted),
            None => substituted,
        }
    }
}

/// Rule 16's host obligation, and the reason `Request` has two renderings: a credential
/// may sit in a url, a header value or a body, so `wire` is what to send and `shown` is
/// what anything else is allowed to see. heklang made these separate fields rather than
/// one field plus a convention precisely so this choice is made once, here, at the
/// compiler's insistence. `heklang/docs/host.md` has the rule.
impl Http for HeklaHost {
    fn send(&mut self, request: &Request) -> Attempt {
        let Some(client) = self.http.clone() else {
            return Attempt::Transport("this world has no network".to_string());
        };
        let headers = match &request.wire.headers {
            Json::Obj(fields) => fields
                .iter()
                .map(|(name, value)| (name.clone(), value::text(&Value::Json(value.clone()))))
                .collect(),
            _ => Vec::new(),
        };
        let body = request
            .wire
            .body
            .as_ref()
            .map(|body| from_heklang_json(body).to_string().into_bytes());
        let sent = HttpRequest {
            method: http_method(request.verb),
            url: request.wire.url.clone(),
            headers,
            body,
        };
        let attempt = client.send(&sent);
        // Counted at the trait boundary rather than inside `UreqClient`, so the number
        // describes the calls an effect made rather than the calls one transport made:
        // a stubbed run counts on the same terms as a real one. This is also the only
        // place that sees an individual attempt, since rule 5 puts the re-send loop
        // inside the language and hands the driver a decided result.
        //
        // Except on a sealed host, which is the same boundary being crossed by something
        // that is not an outbound call at all: a verify replay reaching a call the
        // journal has no entry for is refused by `SealedHttp`, and counting that refusal
        // as a transport failure would report a code divergence as the network being
        // down, in the very series the outbound-failure alert divides by.
        if !self.sealed {
            metrics::effect_http(attempt.as_ref().ok().map(|response| response.status));
        }
        match attempt {
            Ok(response) => {
                // Kept for the driver rather than acted on here. Rule 5 re-sends
                // immediately inside the language, so a limiter that keeps refusing
                // exhausts those attempts and wedges; this is what makes the wait
                // before the *next* invocation the one the server asked for.
                if is_retryable(response.status) {
                    self.retry_after = retry_after_hint(&response.headers);
                }
                // A body that is not JSON is not a transport failure: rule 5 already
                // decided this attempt reached the far side, and the handler sees the
                // status either way.
                let body = serde_json::from_slice::<serde_json::Value>(&response.body)
                    .map(|value| to_heklang_json(&value))
                    .unwrap_or(Json::Null);
                Attempt::Response {
                    status: response.status,
                    body,
                }
            }
            Err(err) => {
                let reason = self.redact_transport(&format!("{err:#}"), request);
                self.last_transport = Some(reason.clone());
                Attempt::Transport(reason)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The journal
// ---------------------------------------------------------------------------

/// One invocation's memory, in the operational database.
///
/// heklang's key is a readable description of the call and hekla stores a hash of it,
/// which is exactly the split `heklang/docs/host.md` section 6 describes: the key stays
/// the language's and the storage stays the host's.
pub struct Journal<'a> {
    pub opdb: &'a Arc<Mutex<OpDb>>,
    pub effect: &'a str,
    pub position: u64,
    pub now: &'a str,
    /// Where this journal publishes the call it is about to answer for, so an `invoke`
    /// appending through the host can key its idempotency tag on it. See
    /// [`HeklaHost::call`].
    pub call: Arc<Mutex<Option<String>>>,
}

impl Calls for Journal<'_> {
    fn recorded(&self, call: &str, ordinal: u32) -> Result<Option<Recorded>, Error> {
        // Published before the answer, because the language asks this immediately
        // before it runs the call. An `invoke` appending through the host reads it back
        // as its idempotency tag; nothing else looks.
        if let Ok(mut held) = self.call.lock() {
            *held = Some(crate::tags::idempotency_tag(
                self.effect,
                &format!(
                    "{}:{}:{ordinal}",
                    self.position,
                    sha256_hex(call.as_bytes())
                ),
            ));
        }
        let found = self
            .opdb
            .lock()
            .map_err(|_| host_error("the operational database lock is poisoned"))?
            .journal_get(
                self.effect,
                self.position,
                &sha256_hex(call.as_bytes()),
                ordinal as u64,
            )
            .map_err(host_error)?;
        let Some(raw) = found else { return Ok(None) };
        let value: serde_json::Value = serde_json::from_str(&raw).map_err(host_error)?;
        Ok(Some(decode_recorded(&value)?))
    }

    fn record(&mut self, call: &str, ordinal: u32, recorded: Recorded) -> Result<(), Error> {
        let (kind, value) = encode_recorded(&recorded);
        self.opdb
            .lock()
            .map_err(|_| host_error("the operational database lock is poisoned"))?
            .journal_put(
                self.effect,
                self.position,
                &sha256_hex(call.as_bytes()),
                ordinal as u64,
                kind,
                &value.to_string(),
                self.now,
            )
            .map_err(host_error)
    }
}

/// A recorded result, as a row. Tagged by kind so a replay reads back the variant that
/// was written rather than guessing from the shape.
fn encode_recorded(recorded: &Recorded) -> (&'static str, serde_json::Value) {
    match recorded {
        Recorded::Response { status, body } => (
            "http",
            serde_json::json!({ "status": status, "body": from_heklang_json(body) }),
        ),
        Recorded::Invoked(outcome) => (
            "invoke",
            serde_json::json!({
                "ok": outcome.ok(),
                "code": outcome.code(),
                "message": outcome.message(),
            }),
        ),
        Recorded::Now(micros) => ("now", serde_json::json!({ "micros": micros })),
        Recorded::Erased => ("erase", serde_json::json!({})),
    }
}

fn decode_recorded(value: &serde_json::Value) -> Result<Recorded, Error> {
    if let Some(micros) = value.get("micros").and_then(serde_json::Value::as_i64) {
        return Ok(Recorded::Now(micros));
    }
    if let Some(status) = value.get("status").and_then(serde_json::Value::as_i64) {
        let body = value.get("body").map_or(Json::Null, to_heklang_json);
        return Ok(Recorded::Response { status, body });
    }
    if let Some(ok) = value.get("ok").and_then(serde_json::Value::as_bool) {
        let code = value.get("code").and_then(serde_json::Value::as_str);
        let message = value.get("message").and_then(serde_json::Value::as_str);
        return Ok(Recorded::Invoked(invoked(ok, code, message)));
    }
    Ok(Recorded::Erased)
}

fn invoked(ok: bool, code: Option<&str>, message: Option<&str>) -> heklang::Invoked {
    match (ok, code, message) {
        (true, _, _) => heklang::Invoked::Ok,
        (false, Some(code), message) => heklang::Invoked::Reject {
            code: code.to_string(),
            message: message.unwrap_or_default().to_string(),
        },
        (false, None, message) => {
            heklang::Invoked::Invalid(message.unwrap_or_default().to_string())
        }
    }
}

impl HeklaHost {
    /// One emitted event as tephra stores it: subject-scoped fields encrypted in the
    /// payload and in their tags, every other indexed field tagged in plaintext, and an
    /// envelope stamped with this run's causation.
    ///
    /// `position` is where the event will land, which only a [`Stamp::Pinned`] world
    /// reads. It is passed rather than looked up because the caller is appending a
    /// batch and this is the only place that knows which of them this is.
    fn lower(
        &mut self,
        event: &Event,
        position: u64,
    ) -> Result<(tephra::Event, EmittedEvent), Error> {
        let ty = schema::event_type(&event.path);
        let def: EventDef = self
            .events
            .get(&ty)
            .cloned()
            .ok_or_else(|| host_error(format!("event type `{ty}` is not declared")))?;

        // The subject ids first: a field scoped to one needs that id's plaintext to
        // find the key, and a declaration may name them in either order.
        let mut ids: BTreeMap<&str, String> = BTreeMap::new();
        for (name, _) in &def.fields {
            if let Some(value) = event.fields.get(name.as_str())
                && let Some(text) = plaintext_scalar(value)
            {
                ids.insert(name.as_str(), text);
            }
        }

        let mut payload = serde_json::Map::new();
        let mut derived: Vec<(String, Option<String>)> = Vec::new();
        for (name, meta) in &def.fields {
            let Some(value) = event.fields.get(name.as_str()) else {
                continue;
            };
            let json = from_heklang_json(&Json::from_value(value));
            match &meta.subject {
                Some(subject_field) => {
                    // Rule 12: an absent optional was never encrypted, so there is no
                    // key behind it and nothing to seal. That is the row which must not
                    // collapse into the erased one.
                    if json.is_null() {
                        payload.insert(name.clone(), serde_json::Value::Null);
                        continue;
                    }
                    let keystore = self.keystore.as_deref().ok_or_else(|| {
                        host_error(format!(
                            "field `{name}` is scoped to `{subject_field}` but no master key is configured"
                        ))
                    })?;
                    let subject_value = ids.get(subject_field.as_str()).ok_or_else(|| {
                        host_error(format!("event `{ty}` has no subject id `{subject_field}`"))
                    })?;
                    let sealed = stored_seal(
                        keystore,
                        subject_field,
                        subject_value,
                        name,
                        &meta.kind,
                        value,
                        &json,
                    )?;
                    if meta.indexed {
                        derived.push((name.clone(), Some(sealed.clone())));
                    }
                    payload.insert(name.clone(), serde_json::Value::String(sealed));
                }
                None => {
                    if meta.indexed
                        && let Some(text) = schema::scalar_to_string(&json)
                    {
                        derived.push((name.clone(), Some(text)));
                    }
                    payload.insert(name.clone(), json);
                }
            }
        }

        let corr = crate::tags::correlation_tag(self.ctx.correlation_id);
        let mut extra: Vec<&str> = vec![corr.as_str()];
        extra.extend(self.idem_tag.as_deref());
        let tags = build_tags(&derived, &extra)?;

        let (event_id, timestamp) = match &self.stamp {
            Stamp::Wall(now) => (uuid::Uuid::new_v4(), now.clone()),
            Stamp::Pinned => (pinned_id(position), pinned_stamp(position)),
        };
        let envelope = envelope::Envelope {
            event_id,
            timestamp,
            correlation_id: self.ctx.correlation_id,
            causation_id: self.ctx.causation_id,
            triggering_event_id: self.ctx.triggering_event_id,
        };
        let data = serde_json::Value::Object(payload);
        let encoded = envelope::encode(&envelope, &data).map_err(host_error)?;
        let event_type = tephra::EventType::new(ty.as_str()).map_err(host_error)?;
        let stored = tephra::Event::new(&event_type, &tags, &encoded).map_err(host_error)?;
        Ok((
            stored,
            EmittedEvent {
                event_type: ty,
                data,
                tags: derived,
            },
        ))
    }
}

/// The plaintext scalar form of a value, or `None` for a container or a seal. A subject
/// id has to be a scalar, which is the same rule `scalar_to_string` applies to a tag,
/// and heklang's rule 12 keeps it out from behind the boundary so there is never a seal
/// here to open.
fn plaintext_scalar(value: &Value) -> Option<String> {
    if matches!(peeled(value), Value::Sealed { .. }) {
        return None;
    }
    schema::scalar_to_string(&from_heklang_json(&Json::from_value(value)))
}

/// The value inside an optional, since `Opt` is outermost around a seal.
fn peeled(value: &Value) -> &Value {
    match value {
        Value::Opt {
            value: Some(held), ..
        } => peeled(held),
        other => other,
    }
}

/// The stored form of a subject-scoped field.
///
/// Two shapes reach a write for one field, and only one of them has content to seal.
/// Fresh plaintext is sealed here, which is the encrypting direction. A `Value::Sealed`
/// was moved from somewhere else and is already stored ciphertext: sealed under this
/// same field and subject it passes through untouched, and under any other name it has
/// to be opened and re-sealed, because [`KeyStore`] binds the field name into the
/// ciphertext and the destination would not be able to read it otherwise.
fn stored_seal(
    keystore: &KeyStore,
    subject_field: &str,
    subject_value: &str,
    name: &str,
    kind: &FieldKind,
    value: &Value,
    json: &serde_json::Value,
) -> Result<String, Error> {
    if let Value::Sealed {
        field,
        subject,
        id,
        content,
    } = peeled(value)
    {
        if field == name && subject == subject_field && id == subject_value {
            return Ok(content.to_string());
        }
        let plaintext = keystore
            .decrypt_subject(subject, id, field, content)
            .map_err(host_error)?
            .ok_or_else(|| {
                host_error(format!(
                    "`{name}` holds content sealed under `{subject}` = `{id}`, whose key is gone, \
                     so it cannot be re-sealed under `{subject_field}`"
                ))
            })?;
        return keystore
            .encrypt_subject(subject_field, subject_value, name, &plaintext)
            .map_err(host_error);
    }
    let text = seal_text(kind, json);
    keystore
        .encrypt_subject(subject_field, subject_value, name, &text)
        .map_err(host_error)
}

/// The text a seal holds, for a value already in the form its store keeps.
///
/// A scalar flattens to its bare text, so the kind alone is enough to read it back as a
/// number or a boolean. A `Json` field is written whole, quotes and all, because it is
/// the one kind whose value can itself be a string that looks like another type:
/// flattening `"42"` to `42` would read back as a number. Its inverse is
/// [`unsealed_json`] for a payload and [`read_api::typed_from_string`] for a column, and
/// both parse a `Json` field for exactly this reason.
fn seal_text(kind: &FieldKind, json: &serde_json::Value) -> String {
    if matches!(kind.base(), FieldKind::Json) {
        return json.to_string();
    }
    schema::scalar_to_string(json).unwrap_or_else(|| json.to_string())
}

fn build_tags(pairs: &[(String, Option<String>)], extra: &[&str]) -> Result<Tags, Error> {
    let mut tags = Vec::with_capacity(pairs.len() + extra.len());
    for (key, value) in pairs {
        let raw = match value {
            Some(value) => format!("{key}:{value}"),
            None => key.clone(),
        };
        tags.push(Tag::new(raw).map_err(host_error)?);
    }
    for raw in extra {
        tags.push(Tag::new((*raw).to_owned()).map_err(host_error)?);
    }
    Tags::new(tags).map_err(host_error)
}

// ---------------------------------------------------------------------------
// Read models
// ---------------------------------------------------------------------------

/// Sealed column writes a fold dropped because the subject's key was gone.
///
/// Counted where the decision is taken, because this is the only place that can tell.
/// [`Rows::put`] writes NULL for a shredded key, and the read model omits a NULL column,
/// so by the time a row is read back an erased column and an optional the handler never
/// wrote are the same absent key. Anything downstream would have to guess, and the guess
/// that says "this row's subject is erased, so its blank column was erased too" is wrong
/// exactly when the column was never written.
///
/// Only the two counts are reported, so only the two counts are kept. A fold over a log
/// whose bulk erasure took a million subjects would otherwise hold a million owned
/// `(String, String)` pairs for the whole scan to answer one `len()`; hashing them keeps
/// distinctness at eight bytes each. A 64-bit collision would undercount one subject in
/// a report, which is not a number anything branches on.
#[derive(Debug, Default)]
pub struct Shredded {
    /// One per dropped column write. A row written three times counts three.
    pub writes: u64,
    /// One hash per distinct `(subject_field, subject_value)` behind those writes.
    seen: HashSet<u64>,
}

impl Shredded {
    /// How many distinct subjects the dropped writes were scoped to.
    pub fn subjects(&self) -> usize {
        self.seen.len()
    }

    fn record(&mut self, subject_field: &str, subject_value: &str) {
        let mut hasher = DefaultHasher::new();
        subject_field.hash(&mut hasher);
        subject_value.hash(&mut hasher);
        self.writes += 1;
        self.seen.insert(hasher.finish());
    }
}

/// One projector's read models, as heklang writes them.
///
/// The crypto is symmetric with the log's: a subject-scoped column is stored as
/// ciphertext and `read_api` decrypts it on the way out, so [`heklang::Rows::put`] encrypts and
/// [`heklang::Rows::row`] decrypts. A stored load in a `patch` therefore sees the plaintext the
/// handler wrote, and a column whose key is gone reads back absent, which is what makes
/// erasure observable through a projection rather than only through the log.
pub struct RowWriter<'a> {
    pub model: &'a ReadModel,
    pub program: &'a Program,
    pub projector: &'a heklang::ir::Projector,
    /// The tables, by entity name.
    pub entities: &'a std::collections::HashMap<String, crate::schema::EntityDef>,
    pub keystore: Option<&'a KeyStore>,
    /// Where to record a sealed column this write had to drop. `None` on every deployed
    /// path: a live projector has nobody to report to, and its rebuild is not a question
    /// anyone asked.
    pub shredded: Option<&'a mut Shredded>,
}

impl RowWriter<'_> {
    fn table(&self, entity: &str) -> Result<&crate::schema::EntityDef, Error> {
        self.entities
            .get(entity)
            .ok_or_else(|| host_error(format!("entity `{entity}` is not declared")))
    }

    fn declared(&self, entity: &str) -> Result<&heklang::ir::EntityDef, Error> {
        self.projector
            .entity(entity)
            .ok_or_else(|| host_error(format!("entity `{entity}` is not declared")))
    }

    fn defs(&self) -> Defs<'_> {
        Defs::in_projector(self.program, self.projector)
    }
}

/// A key as the read model spells it. Every key type is a scalar, so this is the same
/// text a tag or a path segment carries.
fn key_text(key: &heklang::Key) -> String {
    value::text(&heklang::interp::key_as_value(key))
}

impl heklang::host::Rows for RowWriter<'_> {
    fn row(&self, entity: &str, key: &heklang::Key) -> Result<Option<heklang::Row>, Error> {
        let table = self.table(entity)?;
        let declared = self.declared(entity)?;
        let found = self.model.get(table, &key_text(key)).map_err(host_error)?;
        let Some(stored) = found else { return Ok(None) };

        let defs = self.defs();
        let mut row = heklang::Row::default();
        for field in &declared.fields {
            let raw = stored.get(&field.name);
            let subject = table
                .fields
                .iter()
                .find(|(name, _)| name == &field.name)
                .and_then(|(_, meta)| meta.subject.clone());
            let kind = table
                .fields
                .iter()
                .find(|(name, _)| name == &field.name)
                .map(|(_, meta)| meta.kind.clone());
            let json = match (subject, raw) {
                (Some(subject_field), Some(raw)) => {
                    self.decrypt_field(&stored, &subject_field, &field.name, raw, kind.as_ref())?
                }
                (_, Some(raw)) => match kind.as_ref() {
                    Some(kind) => to_heklang_json(&wire_form(kind, raw.clone())),
                    None => to_heklang_json(raw),
                },
                // A column the read model omitted is a NULL, which is an absent
                // optional or a row written before the column existed.
                (_, None) => Json::Null,
            };
            let value = Value::from_json(&json, &field.ty, defs)
                .map_err(|why| Error::new(ErrorKind::Mismatch(why)))?;
            row.0.insert(field.name.clone(), value);
        }
        Ok(Some(row))
    }

    fn put(
        &mut self,
        entity: &heklang::ir::Ident,
        key: heklang::Key,
        row: heklang::Row,
    ) -> Result<(), Error> {
        let table = self.table(entity)?;
        let _ = key;
        // Collected rather than recorded as they are found, so the whole column loop can
        // keep the borrow of `self` that reaching the tables needs.
        let mut dropped: Vec<(String, String)> = Vec::new();

        // The subject ids first, for the same reason the append path needs them: a
        // scoped column is keyed on a sibling's plaintext.
        let mut ids: BTreeMap<&str, String> = BTreeMap::new();
        for (name, value) in &row.0 {
            if let Some(text) = plaintext_scalar(value) {
                ids.insert(name.as_str(), text);
            }
        }

        let mut stored = serde_json::Map::new();
        for (name, meta) in &table.fields {
            let Some(value) = row.0.get(name.as_str()) else {
                continue;
            };
            let json = from_heklang_json(&Json::from_value(value));
            match &meta.subject {
                Some(subject_field) if !json.is_null() => {
                    let keystore = self.keystore.ok_or_else(|| {
                        host_error(format!(
                            "column `{name}` is scoped to `{subject_field}` but no master key is configured"
                        ))
                    })?;
                    let subject_value = ids.get(subject_field.as_str()).ok_or_else(|| {
                        host_error(format!("row has no subject id `{subject_field}`"))
                    })?;
                    // A moved seal is opened here rather than passed through, which the
                    // append path can do. The reason is `column_form` below: a column
                    // stores a `Timestamp` in a different shape than an event does, so a
                    // seal made from the event's form is the wrong text for the column
                    // even when the field name matches. Opening it costs a key use on a
                    // write, where the win of carrying ciphertext was on the read.
                    let json = match peeled(value) {
                        Value::Sealed {
                            field,
                            subject,
                            id,
                            content,
                        } => match keystore
                            .decrypt_subject(subject, id, field, content)
                            .map_err(host_error)?
                        {
                            Some(plaintext) => unsealed_json(&meta.kind, plaintext),
                            // The key is gone, so the column is too. Same answer as the
                            // `_existing` miss below, reached one step earlier.
                            //
                            // Recorded against the *seal's* own subject rather than the
                            // column's. They are usually the same, because a column's
                            // scope is propagated from what is written into it, but the
                            // key that was actually missing is the one this decrypt
                            // asked for, and naming another would report the wrong
                            // subject as erased.
                            None => {
                                stored.insert(name.clone(), serde_json::Value::Null);
                                // Only when somebody is counting. A deployed rebuild
                                // passes no `Shredded`, and over a log with a bulk
                                // erasure this is two owned strings per dropped column,
                                // millions of times, for a tally nothing reads.
                                if self.shredded.is_some() {
                                    dropped.push((subject.to_string(), id.to_string()));
                                }
                                continue;
                            }
                        },
                        _ => json,
                    };
                    // The column form, not the wire form: a sealed column and a plain
                    // one must hold the same shape, or `read_api` would serve a
                    // `Timestamp` as RFC 3339 from one and as epoch micros from the
                    // other depending only on whether it happened to be personal.
                    let stored_json = column_form(&meta.kind, json);
                    let text = seal_text(&meta.kind, &stored_json);
                    // `_existing`, never `encrypt_subject`: that one mints a key when
                    // there is none, and a projection is a read path. Re-projecting a
                    // log whose subject has been erased would otherwise create the very
                    // key the erasure destroyed and write readable content under it,
                    // undoing the shred by rebuilding a read model.
                    match keystore
                        .encrypt_subject_existing(subject_field, subject_value, name, &text)
                        .map_err(host_error)?
                    {
                        Some(sealed) => {
                            stored.insert(name.clone(), serde_json::Value::String(sealed));
                        }
                        // The key is gone, so the column is too. This is the same answer
                        // `read_api` gives a reader, and it is what makes erasure
                        // observable through a projection.
                        None => {
                            stored.insert(name.clone(), serde_json::Value::Null);
                            if self.shredded.is_some() {
                                dropped.push((subject_field.clone(), subject_value.clone()));
                            }
                        }
                    }
                }
                _ => {
                    stored.insert(name.clone(), column_form(&meta.kind, json));
                }
            }
        }

        self.model
            .apply_one(
                table,
                crate::schema::EntityOpKind::Put(serde_json::Value::Object(stored).to_string()),
            )
            .map_err(|err| host_error(format!("applying a write to entity `{entity}`: {err}")))?;
        // After the write, so a row that failed to store is never reported as one whose
        // column was shredded.
        if let Some(shredded) = self.shredded.as_deref_mut() {
            for (field, value) in &dropped {
                shredded.record(field, value);
            }
        }
        Ok(())
    }

    fn delete(&mut self, entity: &heklang::ir::Ident, key: &heklang::Key) -> Result<(), Error> {
        let table = self.table(entity)?;
        self.model
            .apply_one(table, crate::schema::EntityOpKind::Delete(key_text(key)))
            .map_err(host_error)
    }
}

impl RowWriter<'_> {
    /// The same decrypt the log path does, against a row's own subject column.
    ///
    /// A ciphertext is text whatever the column holds, so the declared kind is what
    /// says how to read it back. It goes through the same [`wire_form`] the plain
    /// branch does, because `put` sealed the column form.
    fn decrypt_field(
        &self,
        row: &serde_json::Value,
        subject_field: &str,
        field: &str,
        stored: &serde_json::Value,
        kind: Option<&FieldKind>,
    ) -> Result<Json, Error> {
        let Some(keystore) = self.keystore else {
            return Err(host_error(format!(
                "column `{field}` is scoped to `{subject_field}` but no master key is configured"
            )));
        };
        let Some(ciphertext) = stored.as_str() else {
            return Ok(to_heklang_json(stored));
        };
        let subject_value = row
            .get(subject_field)
            .and_then(crate::schema::scalar_to_string)
            .ok_or_else(|| host_error(format!("row has no subject id `{subject_field}`")))?;
        match keystore
            .decrypt_subject(subject_field, &subject_value, field, ciphertext)
            .map_err(host_error)?
        {
            Some(plaintext) => Ok(match kind {
                Some(kind) => to_heklang_json(&wire_form(
                    kind,
                    read_api::typed_from_string(kind, plaintext),
                )),
                None => Json::Str(plaintext),
            }),
            // The key is gone. Absent is the same answer `read_api` gives a reader,
            // and it is what makes an erased subject observable in a projection.
            None => Ok(Json::Null),
        }
    }
}

/// The subscription for a set of event types: any event of any of them.
///
/// A projector and an effect both subscribe by type alone. What narrows a *fold* is the
/// slice a `state` declares, and that is resolved per run rather than per subscription.
pub fn query_of_types(types: &[String]) -> Result<tephra::Query, Error> {
    let items = types
        .iter()
        .map(|ty| {
            let ty = tephra::EventType::new(ty.as_str()).map_err(host_error)?;
            Ok(QueryItem::new(vec![ty], Tags::new([]).map_err(host_error)?))
        })
        .collect::<Result<Vec<_>, Error>>()?;
    Ok(tephra::Query::items(items))
}

/// One event built from JSON, for a caller that seeds a log rather than running a
/// command.
///
/// The conversion is exactly the one [`record_of`] reverses, so a seeded event and a
/// command's are the same event: same envelope, same tags, same ciphertext.
pub fn event_from_json(
    program: &Program,
    event_type: &str,
    data: &serde_json::Value,
) -> Result<Event, Error> {
    let path = EventPath::new(event_type.split('.'));
    let declared = program
        .event(&path)
        .ok_or_else(|| host_error(format!("event type `{event_type}` is not declared")))?;
    let defs = Defs::of(program);
    let mut fields = BTreeMap::new();
    for field in &declared.fields {
        let json = data.get(&field.name).map_or(Json::Null, to_heklang_json);
        let value = Value::from_json(&json, &field.ty, defs)
            .map_err(|why| Error::new(ErrorKind::Mismatch(why)))?;
        fields.insert(field.name.clone(), value);
    }
    Ok(Event { path, fields })
}

/// Append one event unconditionally, for a caller seeding a log.
///
/// A seeded event goes through the same lowering a command's does, so a test's log and
/// a live one hold the same bytes.
pub fn append_one(host: &mut HeklaHost, event: &Event) -> Result<(), Error> {
    Log::append(
        host,
        slice::from_ref(event),
        &AppendCondition {
            after: 0,
            slices: Vec::new(),
        },
    )
}

/// A decrypted **log payload** seal back as the JSON shape its declaration says it had.
///
/// Sealing flattens everything to text, because that is what a key store takes, so
/// nothing but the field's kind says whether that text was a number or a boolean.
/// heklang's `Value::from_sealed` is the same table for the same reason, keyed on a
/// `Type` where this is keyed on a `FieldKind`.
///
/// **Which producer a seal came from decides which table reads it**, and that is the
/// whole difference between this and [`read_api::typed_from_string`]. A payload seal is
/// made by [`stored_seal`] out of the wire form, so a `Timestamp` in it is micros; a
/// read-model column seal runs through [`column_form`] first, so a `Timestamp` in that
/// one is RFC 3339. Reading one with the other's table types a timestamp as a string.
pub(crate) fn unsealed_json(kind: &FieldKind, text: String) -> serde_json::Value {
    match kind.base() {
        FieldKind::I64 | FieldKind::Timestamp => text
            .parse::<i64>()
            .map_or_else(|_| serde_json::Value::String(text), Into::into),
        FieldKind::Bool if text == "true" => serde_json::Value::Bool(true),
        FieldKind::Bool if text == "false" => serde_json::Value::Bool(false),
        // A record, a list, a map and a `Json` all store as one kind, and [`stored_seal`]
        // flattens a composite with `to_string`. Parsing is the inverse; a scalar that
        // was flattened with `scalar_to_string` instead fails to parse and falls back to
        // the text, which is the right answer for it.
        FieldKind::Json => {
            serde_json::from_str(&text).unwrap_or_else(|_| serde_json::Value::String(text.clone()))
        }
        _ => serde_json::Value::String(text),
    }
}

/// A value in the form its column stores it. `pub(crate)` because `read_model` is the
/// module that stores one, and its own round trip is stated against this form.
///
/// Rule 8's table is about what leaves the process over a socket; a column answers a
/// different question. A `Timestamp` is epoch microseconds on the wire and RFC 3339 in
/// SQLite, because the read API serves that and the column sorts lexicographically on
/// it. Everything else is already in the right shape.
pub(crate) fn column_form(kind: &FieldKind, json: serde_json::Value) -> serde_json::Value {
    match (kind.base(), &json) {
        (FieldKind::Timestamp, serde_json::Value::Number(micros)) => micros
            .as_i64()
            .and_then(rfc3339)
            .map_or(json, serde_json::Value::String),
        _ => json,
    }
}

/// Epoch microseconds as RFC 3339, which is what an envelope and a timestamp column
/// both hold.
fn rfc3339(micros: i64) -> Option<String> {
    let nanos = i128::from(micros) * 1_000;
    time::OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .ok()?
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
}

/// An RFC 3339 timestamp as rule 8's epoch microseconds, or `None` when the text is not
/// one.
///
/// hekla writes a `Timestamp` as RFC 3339 wherever it owns the shape: a read model's
/// column, the read response built from it, and the `date-time` the generated document
/// declares. So it has to read one back wherever a value returns, which is a stored
/// column and a command's own input.
pub(crate) fn timestamp_wire(text: &str) -> Option<serde_json::Value> {
    heklang::value::timestamp(text).map(|micros| serde_json::Value::Number(micros.into()))
}

/// A column's stored form back as rule 8's, which is what `Value::from_json` reads.
///
/// The inverse of [`column_form`], and only a `Timestamp` differs between the two.
fn wire_form(kind: &FieldKind, stored: serde_json::Value) -> serde_json::Value {
    match (kind.base(), &stored) {
        (FieldKind::Timestamp, serde_json::Value::String(text)) => {
            timestamp_wire(text).unwrap_or(stored)
        }
        _ => stored,
    }
}

/// A query item matching any event carrying the idempotency tag.
fn idem_item(tag: &str) -> Result<QueryItem, Error> {
    let tag = Tag::new(tag.to_owned()).map_err(host_error)?;
    Ok(QueryItem::with_tags(Tags::new([tag]).map_err(host_error)?))
}

#[cfg(test)]
mod tests {
    use heklang::Harness;
    use proptest::prelude::*;

    use super::*;
    use crate::propgen;

    fn undecodable(expected: Type, found: &str) -> Unreadable {
        Unreadable {
            event_type: "order.placed".to_owned(),
            fault: Fault::Undecodable {
                position: 1,
                detail: Error::new(ErrorKind::Mismatch(heklang::Mismatch {
                    path: vec!["note".to_owned()],
                    expected,
                    found: found.to_owned(),
                })),
            },
        }
    }

    fn unanswered(sealed: bool) -> Unreadable {
        Unreadable {
            event_type: "order.placed".to_owned(),
            fault: Fault::Unanswered {
                path: "note".to_owned(),
                sealed,
            },
        }
    }

    /// One sentence per distinct repair, not per event. A deploy that broke two event
    /// types the same way has one thing to fix; `tests/absent.rs` pins each repair
    /// alone, and this pins the combinations a fixture cannot easily produce.
    #[test]
    fn guidance_is_one_sentence_per_repair_however_many_events_call_for_it() {
        let absences = [unanswered(false), undecodable(Type::String, "nothing")];
        let guidance = unreadable_guidance(&absences);
        assert_eq!(guidance.len(), 1, "both are the same repair: {guidance:?}");
        assert!(guidance[0].contains("@absent(<value>)"), "{guidance:?}");

        let four = [
            unanswered(false),
            unanswered(true),
            undecodable(Type::String, "a number"),
            Unreadable {
                event_type: "order.placed".to_owned(),
                fault: Fault::Undecodable {
                    position: 1,
                    detail: host_error("envelope timestamp `x` is not RFC 3339"),
                },
            },
        ];
        let guidance = unreadable_guidance(&four);
        assert_eq!(guidance.len(), 4, "{guidance:?}");
        assert!(
            guidance[1].contains("cannot take `@absent`"),
            "{guidance:?}"
        );
        assert!(guidance[2].contains("under a new name"), "{guidance:?}");
        assert!(
            guidance[3].contains("no edit to a `.hk` file repairs it"),
            "a payload hekla could not read at all is not a declaration fault: {guidance:?}"
        );

        assert!(unreadable_guidance(&[]).is_empty());
    }

    /// A field added as `@subject(...)` is an absence like any other, but the repair the
    /// plain absence gets is one heklang refuses outright on sealed content, so it must
    /// not be the one offered.
    #[test]
    fn a_sealed_field_is_never_told_to_take_an_absent_value() {
        for one in [
            unanswered(true),
            undecodable(Type::sealed(Type::String, "guest_id".to_owned()), "nothing"),
        ] {
            assert!(one.is_absence(), "it is still an absence");
            let guidance = unreadable_guidance(std::slice::from_ref(&one));
            assert!(guidance[0].contains("make it optional"), "{guidance:?}");
            assert!(
                !guidance[0].contains("`@absent(<value>)`"),
                "heklang refuses `@absent` on sealed content: {guidance:?}"
            );
        }
    }

    /// The wire form of a value: rule 8's table, which is what every conversion below
    /// starts from and has to return to.
    fn wire(value: &Value) -> serde_json::Value {
        from_heklang_json(&Json::from_value(value))
    }

    /// `Json` is the kind whose value can be a string that looks like another type, and
    /// the seal loop is where the quotes saying which would be dropped. Pinned by name
    /// rather than left to the generator, which reaches it about a third of the time.
    #[test]
    fn a_sealed_json_string_that_looks_like_a_number_stays_a_string() {
        let kind = FieldKind::Json;
        let value = Value::Json(Json::Str("42".to_owned()));
        let wire = wire(&value);
        assert_eq!(seal_text(&kind, &wire), "\"42\"");
        assert_eq!(unsealed_json(&kind, seal_text(&kind, &wire)), wire);
        assert_eq!(
            read_api::typed_from_string(&kind, seal_text(&kind, &wire)),
            wire
        );
    }

    /// A scalar of any other kind flattens bare, so its text is the value and not a
    /// quoted one. This is the other half of the rule above, and it is what makes a
    /// sealed integer read back as a number.
    #[test]
    fn a_sealed_scalar_flattens_without_quotes() {
        assert_eq!(seal_text(&FieldKind::I64, &serde_json::json!(42)), "42");
        assert_eq!(
            seal_text(
                &FieldKind::Text { max_length: None },
                &serde_json::json!("42")
            ),
            "42"
        );
        assert_eq!(
            seal_text(&FieldKind::Bool, &serde_json::json!(true)),
            "true"
        );
    }

    proptest! {
        /// A column stores what a socket would send, in one of two shapes, and the pair
        /// has to be a bijection or a `Timestamp` reads back as whatever it was stored
        /// as. Only `Timestamp` differs between the two forms today; the property is
        /// over every kind so that an arm added to one and not the other is caught.
        #[test]
        fn a_column_form_reads_back_as_the_wire_form_it_was_made_from(
            (ty, value) in propgen::typed_value()
        ) {
            let kind = propgen::kind_of(&ty);
            let wire = wire(&value);
            let column = column_form(&kind, wire.clone());
            prop_assert_eq!(wire_form(&kind, column), wire);
        }

        /// A seal flattens to text, so the kind is the only thing that says what the
        /// text was. This is the log-payload half of the loop: `stored_seal` writes,
        /// `unsealed_json` reads.
        ///
        /// Nulls are out of scope by rule 12: an absent optional is never sealed, so
        /// there is no key behind it and nothing for this to invert.
        #[test]
        fn a_payload_seal_reads_back_as_the_wire_form_it_sealed(
            (ty, value) in propgen::typed_value()
        ) {
            let kind = propgen::kind_of(&ty);
            let wire = wire(&value);
            prop_assume!(!wire.is_null());
            prop_assert_eq!(unsealed_json(&kind, seal_text(&kind, &wire)), wire);
        }

        /// The column half of the same loop: `RowWriter::put` seals the column form and
        /// `read_api` re-types it. A `Timestamp` is RFC 3339 on both sides here, which
        /// is why this reads with a different table than the payload above.
        #[test]
        fn a_column_seal_reads_back_as_the_column_form_it_sealed(
            (ty, value) in propgen::typed_value()
        ) {
            let kind = propgen::kind_of(&ty);
            let column = column_form(&kind, wire(&value));
            prop_assume!(!column.is_null());
            prop_assert_eq!(
                read_api::typed_from_string(&kind, seal_text(&kind, &column)),
                column
            );
        }

        /// serde is what the envelope and the socket speak, `Json` is what rule 8 is
        /// written against, and a value crossing the two has to come back itself.
        ///
        /// Stated over values that have already been through `from_heklang_json`, which
        /// is what a parser would have produced. That direction is an identity; the
        /// other one normalises number text, and the cases below pin how.
        #[test]
        fn a_parsed_json_value_survives_the_heklang_bridge(json in propgen::json()) {
            let parsed = from_heklang_json(&json);
            prop_assert_eq!(from_heklang_json(&to_heklang_json(&parsed)), parsed);
        }

        /// The other direction of the same table. heklang holds a number as the text it
        /// was written with, so this is idempotence rather than identity: anything
        /// already in serde's normal form is unchanged, and anything else reaches it in
        /// one pass and stays.
        #[test]
        fn the_heklang_bridge_normalises_a_number_once_and_then_leaves_it(
            json in propgen::json()
        ) {
            let once = to_heklang_json(&from_heklang_json(&json));
            let twice = to_heklang_json(&from_heklang_json(&once));
            prop_assert_eq!(twice, once);
        }

        /// Rule 8's table met from both ends. The reader takes the declared type rather
        /// than inferring one from the JSON, so this is the property that says the two
        /// directions agree about what that type means.
        #[test]
        fn the_conversion_table_round_trips((ty, value) in propgen::typed_value()) {
            let written = Json::from_value(&value);
            let read = Value::from_json(&written, &ty, propgen::defs());
            prop_assert_eq!(read, Ok(value));
        }
    }

    /// What serde's parser does to a number's text, written down. `arbitrary_precision`
    /// keeps the digits, so these are the whole of the normalisation and the reason the
    /// property above is idempotence rather than identity.
    #[test]
    fn a_number_normalises_the_way_serde_parses_one() {
        let round = |text: &str| from_heklang_json(&Json::num(text)).to_string();
        // An unsigned exponent gains a sign, and `E` lowercases.
        assert_eq!(round("1e2"), "1e+2");
        assert_eq!(round("1E5"), "1e+5");
        assert_eq!(round("1e-2"), "1e-2");
        // Negative zero parses as an i64, and an i64 has one zero.
        assert_eq!(round("-0"), "0");
        // Not a float: the trailing zero and the extra digits are what
        // `arbitrary_precision` is carried for, and they survive.
        assert_eq!(round("10.50"), "10.50");
        assert_eq!(round("-0.0"), "-0.0");
        assert_eq!(
            round("123456789012345678901234567890"),
            "123456789012345678901234567890"
        );
    }

    /// `Json::num` does not validate, so text that is not a number changes JSON *type*
    /// on the way across rather than failing. Unreachable from hekla, which only builds
    /// a `Num` out of a `serde_json::Number`; pinned so that stays true by test rather
    /// than by comment.
    #[test]
    fn a_number_that_is_not_one_crosses_as_a_string() {
        assert_eq!(
            from_heklang_json(&Json::num("not a number")),
            serde_json::Value::String("not a number".to_owned())
        );
    }

    /// The generator stops at year 0000 and year 9999 because that is where
    /// `column_form` stops being able to render a timestamp, and past it every table
    /// keyed on `Timestamp` falls through to a different answer. Pinned here so the
    /// bound is a documented behaviour rather than an unexplained constant in the
    /// generator.
    #[test]
    fn a_timestamp_outside_the_renderable_range_stops_being_a_timestamp() {
        let kind = FieldKind::Timestamp;
        for micros in [
            i64::MIN,
            i64::MAX,
            propgen::MIN_MICROS - 1,
            propgen::MAX_MICROS + 1,
        ] {
            let wire = serde_json::json!(micros);
            let column = column_form(&kind, wire.clone());
            // `rfc3339` declines, so the column keeps a JSON number where its own SQL
            // type is TEXT. `wire_form` leaves a number alone, so the pair still round
            // trips; what breaks is everything downstream that expected text.
            assert_eq!(column, wire, "{micros} should pass through unrendered");
            assert_eq!(wire_form(&kind, column.clone()), wire);
            // The seal loop is where it shows: the digits flatten and read back as text.
            assert_eq!(
                read_api::typed_from_string(&kind, seal_text(&kind, &column)),
                serde_json::Value::String(micros.to_string()),
                "an unrendered timestamp seals as digits and reads back as text"
            );
        }
    }

    /// Inside the range, both ends render and parse back exactly. Micros are exact in
    /// nanoseconds and `value::timestamp` truncates to six digits, so nothing is lost
    /// at the boundary itself.
    #[test]
    fn a_timestamp_at_either_end_of_the_range_round_trips() {
        let kind = FieldKind::Timestamp;
        for micros in [propgen::MIN_MICROS, propgen::MAX_MICROS] {
            let wire = serde_json::json!(micros);
            let column = column_form(&kind, wire.clone());
            assert!(column.is_string(), "{micros} should render: {column}");
            assert_eq!(wire_form(&kind, column), wire);
        }
    }

    /// The one value an optional cannot tell from absence. A `Json?` holding a JSON
    /// null writes `null`, which is also what `none` writes, and the reader has to pick
    /// one. It picks `none`.
    ///
    /// Inherent rather than fixable: `null` is the only wire form either has. Worth
    /// pinning because it is the single exception to the round trip above, and because
    /// a `Json?` column that quietly reads back empty is otherwise a long afternoon.
    #[test]
    fn a_json_null_inside_an_optional_reads_back_as_absent() {
        let ty = heklang::ir::Type::opt(heklang::ir::Type::Json);
        let value = Value::some(Value::Json(Json::Null));
        assert_eq!(Json::from_value(&value), Json::Null);
        assert_eq!(
            Value::from_json(&Json::Null, &ty, propgen::defs()),
            Ok(Value::none(heklang::ir::Type::Json))
        );
    }

    /// `serde_json::Map` is a `BTreeMap` here, and two things depend on it: an object
    /// surviving the bridge above, and `verify::compare_entity` comparing rows as JSON
    /// text. Turning on `preserve_order`, which feature unification could do from any
    /// dependency, would break both quietly.
    #[test]
    fn a_json_object_is_ordered_by_key_and_not_by_insertion() {
        let parsed: serde_json::Value = serde_json::from_str(r#"{"b":1,"a":2}"#).unwrap();
        assert_eq!(parsed.to_string(), r#"{"a":2,"b":1}"#);
    }
    /// One event, since nothing here reads a payload: what is under test is the
    /// envelope a world synthesises around it.
    fn event(n: u64) -> Event {
        Event::new(
            EventPath::new(["thing", "happened"]),
            [("n", Value::Int(n as i64))],
        )
    }

    /// The test the whole [`Stamp::Pinned`] arm exists to satisfy, asked of the
    /// reference implementation rather than of a number written down twice.
    ///
    /// `PINNED_EPOCH_MICROS` and the step beside it are copies of constants that are
    /// private to `heklang::Harness`, so hekla is coupled to that crate's internals and
    /// not merely to its published contract. This is the whole mitigation: a heklang
    /// release that moves either one fails here, loudly and in hekla's own suite,
    /// instead of surfacing much later as a downstream project whose `hek test` and
    /// `hekla test` disagree by an amount nobody recognises.
    #[test]
    fn a_pinned_envelope_is_the_one_heklangs_own_harness_writes() {
        let mut harness = Harness::default();
        for n in 0..12 {
            harness.push(event(n));
        }

        for position in 0..12 {
            let record = harness
                .record(position)
                .expect("the harness reads its own log")
                .expect("a record it just pushed");
            assert_eq!(
                record.at,
                pinned_at(position),
                "the append time at position {position}"
            );
            assert_eq!(
                record.id,
                pinned_id(position).to_string(),
                "the event id at position {position}"
            );
        }

        // Twelve rather than two, because the id's counter is decimal in a field read
        // as hex: everything below ten agrees under either reading, and only position
        // ten tells the two apart.
        assert_eq!(
            harness.record(10).unwrap().unwrap().id,
            "0190d1a1-0000-7000-9000-000000000010"
        );

        // heklang derives `now()` from the log's length, so it reads as the instant the
        // next append will be stamped with. That is what makes an emitted event's `at`
        // equal to the `now()` its own command saw.
        assert_eq!(Clock::now(&harness), pinned_at(12));
    }

    /// The stamp is what an envelope holds and `pinned_at` is what a record reads back
    /// as, so a divergence between the two would put `e.at` a rounding away from
    /// `now()` in a language where they are meant to meet exactly.
    #[test]
    fn a_pinned_stamp_reads_back_as_the_instant_it_was_written_for() {
        for position in [0, 1, 10, 1_440] {
            assert_eq!(
                value::timestamp(&pinned_stamp(position)),
                Some(pinned_at(position))
            );
        }
        assert_eq!(pinned_stamp(0), "2020-01-01T00:00:00Z");
        assert_eq!(pinned_stamp(1), "2020-01-01T00:01:00Z");
    }
}
