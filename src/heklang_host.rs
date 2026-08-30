//! The world hekla hands heklang: tephra for the log, the key store for subjects,
//! an HTTP client for the network, and the operational database for a journal.
//!
//! Everything here is a conversion. heklang decides what a program means and this
//! decides what that costs in storage, so the two models meet exactly once, at this
//! seam, and neither reshapes itself to suit the other.
//!
//! **Crypto lives below this file.** heklang models a seal logically: a `Value::Sealed`
//! wraps plaintext and only `reveal` can read it out, and `Keys` answers nothing but
//! whether a subject is erased (`heklang/docs/host.md` section 10 records ciphertext as
//! a gap). hekla really encrypts, so [`Log::read`] decrypts a subject-scoped field on
//! the way in and [`Log::append`] encrypts it on the way out. The language never sees a
//! ciphertext and the store never sees a plaintext.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use heklang::host::{
    AppendCondition, Attempt, Calls, Clock, Http, Keys, Log, Predicate, Query, Recorded, Request,
};
use heklang::interp::{Error, ErrorKind};
use heklang::ir::{EventPath, Type};
use heklang::value::{self, Defs};
use heklang::{Event, Json, Program, Record, Value};
use tephra::{Position, QueryItem, Tag, Tags, WriteHandle};

use crate::context::CommandContext;
use crate::crypto::KeyStore;
use crate::envelope;
use crate::hash::sha256_hex;
use crate::http::{HttpClient, HttpRequest};
use crate::opdb::OpDb;
use crate::read_api;
use crate::read_model::ReadModel;
use crate::schema::{self, EmittedEvent, EventDef, EventDefs, FieldKind};

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

/// One request's or one invocation's world.
///
/// Built per run rather than shared: it carries the causation metadata and the pinned
/// append time of the thing being run, which is exactly the scope tephra's writer is
/// not.
pub struct HeklaHost {
    pub program: Arc<Program>,
    pub events: Arc<EventDefs>,
    pub store: WriteHandle,
    pub keystore: Option<Arc<KeyStore>>,
    /// Causation for the events this run appends. heklang has no opinion on it, which
    /// is why it stays hekla's.
    pub ctx: CommandContext,
    /// The append time, RFC 3339. heklang pins its own `now()` per invocation through
    /// [`Clock`]; this is what the envelope stamps.
    pub now: String,
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
    /// A deterministic id source, for a world that has to be reproducible. The nth
    /// event appended gets `…-00000000000n`, so an id derived from `e.id` can be
    /// written down in a test. `None` mints a v4, which is what a live append does.
    pub minted: Option<u32>,
}

impl HeklaHost {
    /// One stored event as heklang reads it.
    fn record_of(&self, position: Position, event: tephra::EventRef<'_>) -> Result<Record, Error> {
        record_of(
            &self.program,
            &self.events,
            self.keystore.as_deref(),
            position,
            event,
        )
    }
}

/// One stored event as heklang reads it: subject fields decrypted, every field typed by
/// its declaration.
///
/// Free rather than a method because a projector thread reads the log without a
/// [`HeklaHost`]: it has no clock, no network and nothing to append.
pub fn record_of(
    program: &Program,
    events: &EventDefs,
    keystore: Option<&KeyStore>,
    position: Position,
    event: tephra::EventRef<'_>,
) -> Result<Record, Error> {
    let ty = event.event_type();
    let path = EventPath::new(ty.split('.'));
    let declared = program
        .event(&path)
        .ok_or_else(|| host_error(format!("event type `{ty}` is not declared")))?;
    let schema = events.get(ty);
    let (envelope, data) = envelope::decode(event.data()).map_err(host_error)?;

    let defs = Defs::of(program);
    let mut fields = BTreeMap::new();
    for field in &declared.fields {
        let stored = data.get(&field.name);
        let subject = schema
            .and_then(|schema| schema.field(&field.name))
            .and_then(|meta| meta.subject.clone());
        let json = match (subject, stored) {
            (Some(subject_field), Some(stored)) => decrypt_field(
                keystore,
                &data,
                &subject_field,
                &field.name,
                stored,
                &field.ty,
            )?,
            (_, Some(stored)) => Some(to_heklang_json(stored)),
            (_, None) => Some(Json::Null),
        };
        let value = match json {
            // The declared type, seal and all: `Value::from_json` reads a seal
            // transparently and heklang re-seals the value as it binds it, so the
            // content is behind `reveal` from the moment it enters a frame.
            Some(json) => Value::from_json(&json, &field.ty, defs)
                .map_err(|why| Error::new(ErrorKind::Mismatch(why)))?,
            // Stored, and not readable: the subject's key is gone.
            //
            // This must not come back as `none`. Rule 12 turns on absent and erased
            // being different rows, and an optional that read absent would let a
            // handler take the "there was never an address here" branch for a customer
            // who had one. A placeholder keeps the value present, so the program
            // reaches `reveal`, and `reveal` is what consults `Keys::erased` and fails
            // terminally. The content is unrecoverable either way; which branch runs is
            // not.
            None => unreadable(&field.ty, defs),
        };
        fields.insert(field.name.clone(), value);
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

/// Decrypt one subject-scoped field, keyed on its sibling subject id.
///
/// An unreadable value is absent rather than an error, which is the same answer
/// `read_api` gives a reader: the key is gone. heklang seals whatever it gets and
/// `reveal` consults `Keys::erased` before it looks, so an erased subject can never be
/// read out no matter what stands in for it here.
fn decrypt_field(
    keystore: Option<&KeyStore>,
    data: &serde_json::Value,
    subject_field: &str,
    field: &str,
    stored: &serde_json::Value,
    ty: &Type,
) -> Result<Option<Json>, Error> {
    let Some(keystore) = keystore else {
        return Err(host_error(format!(
            "field `{field}` is scoped to `{subject_field}` but no master key is configured"
        )));
    };
    let Some(ciphertext) = stored.as_str() else {
        return Ok(Some(to_heklang_json(stored)));
    };
    let subject_value = data
        .get(subject_field)
        .and_then(schema::scalar_to_string)
        .ok_or_else(|| host_error(format!("event has no subject id `{subject_field}`")))?;
    match keystore
        .decrypt_subject(subject_field, &subject_value, field, ciphertext)
        .map_err(host_error)?
    {
        Some(plaintext) => Ok(Some(sealed_json(plaintext, ty))),
        // Not an absence: the ciphertext is right there and the key is not.
        None => Ok(None),
    }
}

/// A decrypted plaintext back as rule 8's JSON, against the field's declared type.
///
/// Encryption takes a string, so [`lower`] seals [`schema::scalar_to_string`] of the
/// rule 8 form and that flattens a number and a boolean into text. Only the
/// declaration says which of the two it was, so only the declaration can read it back.
fn sealed_json(text: String, ty: &Type) -> Json {
    match ty {
        Type::Opt(inner) | Type::Sealed(inner, _) => sealed_json(text, inner),
        Type::Int | Type::Timestamp => text.parse::<i64>().map_or(Json::Str(text), Json::int),
        Type::Bool => match text.as_str() {
            "true" => Json::Bool(true),
            "false" => Json::Bool(false),
            _ => Json::Str(text),
        },
        Type::Json | Type::List(_) | Type::Map(_, _) | Type::Record(_) => {
            serde_json::from_str::<serde_json::Value>(&text)
                .map_or(Json::Str(text), |value| to_heklang_json(&value))
        }
        // A String, a Uuid, a Money or Decimal at its scale, and an enum variant are
        // all text on the wire, so the seal held exactly what goes back.
        _ => Json::Str(text),
    }
}

/// What stands in for content whose key is gone, so the value stays *present*.
///
/// Never read: rule 12 lets sealed content be moved, asked whether it is there, and
/// revealed, and `reveal` fails before this is looked at. It exists so the shape is
/// right, not so the content is.
fn unreadable(ty: &Type, defs: Defs<'_>) -> Value {
    match ty {
        // `Opt` is outermost, so a sealed optional keeps its `Some`.
        Type::Opt(inner) => Value::Opt {
            inner: inner.as_ref().clone(),
            value: Some(Box::new(unreadable(inner, defs))),
        },
        Type::Sealed(inner, _) => unreadable(inner, defs),
        other => value::zero(other, defs).unwrap_or_else(|| Value::str("")),
    }
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
        // A command that decided to do nothing still commits, and heklang appends its
        // (empty) outcome rather than special-casing it. There is nothing to write and
        // nothing a condition could guard, so this is where that stops.
        if events.is_empty() {
            return Ok(());
        }
        // An attempt that loses the race lowers the same events again on the next one,
        // and `lower` mints an id per event. Rewinding the counter is what keeps the ids
        // a run assigns independent of how many times it was beaten to the log. Nothing
        // reaches it today (`hek test` is the only deterministic minter and it is
        // single-threaded), and a counter that drifts on a retry is the kind of thing
        // that is found much later, from the wrong end.
        let minter = self.minted;
        let mut built = Vec::with_capacity(events.len());
        let mut emitted = Vec::with_capacity(events.len());
        for event in events {
            let (stored, reported) = self.lower(event)?;
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
        let landed = self.store.append(built, Some(dcb));
        if landed.is_err() {
            self.minted = minter;
        }
        match landed {
            Ok(range) => {
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

impl Clock for HeklaHost {
    fn now(&self) -> i64 {
        value::timestamp(&self.now).unwrap_or(0)
    }
}

impl Keys for HeklaHost {
    fn erased(&self, subject: &str, id: &str) -> Result<bool, Error> {
        let Some(keystore) = self.keystore.as_deref() else {
            return Ok(false);
        };
        keystore.erased(subject, id).map_err(host_error)
    }

    fn erase(&mut self, subject: &str, id: &str) -> Result<(), Error> {
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

impl Http for HeklaHost {
    fn send(&mut self, request: &Request) -> Attempt {
        let Some(client) = self.http.clone() else {
            return Attempt::Transport("this world has no network".to_string());
        };
        let headers = match &request.headers {
            Json::Obj(fields) => fields
                .iter()
                .map(|(name, value)| (name.clone(), value::text(&Value::Json(value.clone()))))
                .collect(),
            _ => Vec::new(),
        };
        let body = request
            .body
            .as_ref()
            .map(|body| from_heklang_json(body).to_string().into_bytes());
        let sent = HttpRequest {
            method: http_method(request.verb),
            url: request.url.clone(),
            headers,
            body,
        };
        match client.send(&sent) {
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
                let reason = format!("{err:#}");
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
    fn lower(&mut self, event: &Event) -> Result<(tephra::Event, EmittedEvent), Error> {
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
            let bare = value.clone().unsealed();
            let json = from_heklang_json(&Json::from_value(&bare));
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
                    let text = schema::scalar_to_string(&json).unwrap_or_else(|| json.to_string());
                    let sealed = keystore
                        .encrypt_subject(subject_field, subject_value, name, &text)
                        .map_err(host_error)?;
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

        let event_id = match &mut self.minted {
            Some(seen) => {
                *seen += 1;
                uuid::Uuid::from_u128(u128::from(*seen))
            }
            None => uuid::Uuid::new_v4(),
        };
        let envelope = envelope::Envelope {
            event_id,
            timestamp: self.now.clone(),
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

/// The plaintext scalar form of a value, or `None` for a container. A subject id has to
/// be a scalar, which is the same rule `scalar_to_string` applies to a tag.
fn plaintext_scalar(value: &Value) -> Option<String> {
    let bare = value.clone().unsealed();
    schema::scalar_to_string(&from_heklang_json(&Json::from_value(&bare)))
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

/// One projector's read models, as heklang writes them.
///
/// The crypto is symmetric with the log's: a subject-scoped column is stored as
/// ciphertext and `read_api` decrypts it on the way out, so [`Rows::put`] encrypts and
/// [`Rows::row`] decrypts. A stored load in a `patch` therefore sees the plaintext the
/// handler wrote, and a column whose key is gone reads back absent, which is what makes
/// erasure observable through a projection rather than only through the log.
pub struct RowWriter<'a> {
    pub model: &'a ReadModel,
    pub program: &'a Program,
    pub projector: &'a heklang::ir::Projector,
    /// The tables, by entity name.
    pub entities: &'a std::collections::HashMap<String, crate::schema::EntityDef>,
    pub keystore: Option<&'a KeyStore>,
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
            let bare = value.clone().unsealed();
            let json = from_heklang_json(&Json::from_value(&bare));
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
                    // The column form, not the wire form: a sealed column and a plain
                    // one must hold the same shape, or `read_api` would serve a
                    // `Timestamp` as RFC 3339 from one and as epoch micros from the
                    // other depending only on whether it happened to be personal.
                    let stored_json = column_form(&meta.kind, json);
                    let text = crate::schema::scalar_to_string(&stored_json)
                        .unwrap_or_else(|| stored_json.to_string());
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
            .map_err(|err| host_error(format!("applying a write to entity `{entity}`: {err}")))
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
        std::slice::from_ref(event),
        &AppendCondition {
            after: 0,
            slices: Vec::new(),
        },
    )
}

/// A value in the form its column stores it.
///
/// Rule 8's table is about what leaves the process over a socket; a column answers a
/// different question. A `Timestamp` is epoch microseconds on the wire and RFC 3339 in
/// SQLite, because the read API serves that and the column sorts lexicographically on
/// it. Everything else is already in the right shape.
fn column_form(kind: &FieldKind, json: serde_json::Value) -> serde_json::Value {
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

/// A column's stored form back as rule 8's, which is what `Value::from_json` reads.
///
/// The inverse of [`column_form`], and only a `Timestamp` differs between the two.
fn wire_form(kind: &FieldKind, stored: serde_json::Value) -> serde_json::Value {
    match (kind.base(), &stored) {
        (FieldKind::Timestamp, serde_json::Value::String(text)) => heklang::value::timestamp(text)
            .map_or(stored, |micros| serde_json::Value::Number(micros.into())),
        _ => stored,
    }
}

/// A query item matching any event carrying the idempotency tag.
fn idem_item(tag: &str) -> Result<QueryItem, Error> {
    let tag = Tag::new(tag.to_owned()).map_err(host_error)?;
    Ok(QueryItem::with_tags(Tags::new([tag]).map_err(host_error)?))
}
