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

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use heklang::host::{
    AppendCondition, Attempt, Calls, Clock, Http, Keys, Log, Predicate, Query, Recorded, Request,
};
use heklang::interp::{Error, ErrorKind};
use heklang::ir::EventPath;
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

/// One stored event as heklang reads it: every field typed by its declaration, and a
/// subject-scoped one still sealed.
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
        let value = match data.get(&field.name) {
            Some(stored) => Value::from_json(&to_heklang_json(stored), &field.ty, defs)
                .map_err(|why| Error::new(ErrorKind::Mismatch(why)))?,
            None => Value::from_json(&Json::Null, &field.ty, defs)
                .map_err(|why| Error::new(ErrorKind::Mismatch(why)))?,
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
        if self.sealed {
            return Err(host_error("a sealed replay tried to append to the log"));
        }
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
                            None => {
                                stored.insert(name.clone(), serde_json::Value::Null);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::propgen;
    use proptest::prelude::*;

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
}
