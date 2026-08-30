//! Running one command: bind its arguments, let heklang decide, and retry a conflict.
//!
//! heklang has no `Conflict` outcome on purpose (`heklang/docs/host.md` section 5):
//! being beaten to the log is not one of the three answers a command can give. So the
//! decision belongs to the language, the *policy* for what to do about a conflict
//! belongs here (how many attempts, and how long to wait), and the attempts themselves
//! happen inside `run_retrying`, which is the only place that can fold a retry onto the
//! state the last attempt built instead of re-reading the whole boundary.

use std::sync::Arc;

use anyhow::anyhow;
use heklang::interp::ErrorKind;
use heklang::value::Defs;
use heklang::{Interpreter, Outcome, Program, Value};
use tephra::{Position, PositionRange, Query, QueryItem, Tag, Tags, WriteHandle};
use uuid::Uuid;

use crate::context::CommandContext;
use crate::crypto::KeyStore;
use crate::envelope;
use crate::heklang_host::{HeklaHost, to_heklang_json};
use crate::schema::{EmittedEvent, EventDefs};
use crate::tags::RESERVED_TAG_PREFIX;

/// A command outcome recovered from the log by its idempotency tag: a prior committed
/// attempt of the same request, so a replay returns it rather than re-deciding.
pub struct RecoveredOutcome {
    pub events: Vec<RecoveredEvent>,
    pub positions: PositionRange,
    pub correlation_id: Uuid,
    pub causation_id: Uuid,
}

/// One recovered event: its type and its derived tags (reserved host tags stripped),
/// already rendered as the `"key:value"` strings the response reports.
pub struct RecoveredEvent {
    pub event_type: String,
    pub tags: Vec<String>,
}

/// The outcome of one command attempt.
pub enum CommandOutcome {
    /// The command emitted events (possibly none) and they were appended.
    Committed {
        events: Vec<EmittedEvent>,
        /// The assigned positions, or `None` when nothing was emitted.
        positions: Option<PositionRange>,
    },
    /// The command refused on state grounds; nothing was written.
    Rejected { code: String, message: String },
    /// The input was malformed; nothing was written.
    InvalidInput { message: String },
    /// The append hit a concurrent write inside the boundary. The caller retries.
    Conflict,
    /// This request already committed under its idempotency tag (a crashed or
    /// concurrent duplicate): the outcome was recovered from the log rather than
    /// re-decided, and the caller returns it verbatim.
    AlreadyCommitted(RecoveredOutcome),
    /// The store could not service the append for a transient reason (the write
    /// coordinator is draining). The caller surfaces a retryable status.
    Unavailable { message: String },
}

/// How a DCB conflict is retried: how many attempts, and what to do between them.
///
/// The policy belongs to the caller (the runtime picks the budget and the backoff) but
/// the loop belongs here, so the work that does not vary between attempts is done once
/// per request rather than once per attempt.
pub struct Retry<'a> {
    /// Total attempts including the first. Below 1, nothing runs and the command
    /// reports a conflict.
    pub max_attempts: u32,
    /// Called before each retry with the zero-based number of the attempt that just
    /// conflicted.
    pub backoff: &'a dyn Fn(u32),
}

impl Retry<'_> {
    /// One attempt and no backoff: a conflict is reported rather than retried. What
    /// `hek test` wants, where the log is seeded and nothing else is writing.
    pub fn once() -> Retry<'static> {
        Retry {
            max_attempts: 1,
            backoff: &|_| {},
        }
    }
}

/// Bind a request body to a command's declared parameters.
///
/// A missing key reads as `null`, so an absent optional is absent and an absent
/// required parameter is the mismatch it actually is. This is the whole of host-side
/// input validation now: the declaration is the schema, and heklang's own conversion
/// is what enforces it.
fn bind_args(
    command: &heklang::ir::Command,
    defs: Defs<'_>,
    input: &serde_json::Value,
) -> Result<Vec<(String, Value)>, String> {
    // A key the command does not declare is a typo far more often than it is spare
    // data, and it is the one thing binding by name cannot notice on its own: heklang
    // reads the parameters it knows and never looks at the rest.
    if let Some(object) = input.as_object() {
        for name in object.keys() {
            if !command.params.iter().any(|param| &param.name == name) {
                return Err(format!("unknown field `{name}`"));
            }
        }
    }

    let mut args = Vec::with_capacity(command.params.len());
    for param in &command.params {
        let found = input.get(&param.name);
        // An absent key and a key holding the wrong thing are different mistakes, and
        // "missing required field" is the more useful of the two answers. An optional
        // parameter takes the absence as `none` and falls through to the conversion.
        if found.is_none() && !matches!(param.ty, heklang::ir::Type::Opt(_)) {
            return Err(format!("missing required field `{}`", param.name));
        }
        let json = found.map_or(heklang::Json::Null, to_heklang_json);
        let value = Value::from_json(&json, &param.ty, defs)
            .map_err(|why| format!("`{}`: {why}", param.name))?;
        args.push((param.name.clone(), value));
    }
    Ok(args)
}

/// Whether a body is well formed for a command, without running it. The runtime checks
/// once before dispatch so an attempt never has to.
pub fn validate_input(
    program: &Program,
    name: &str,
    input: &serde_json::Value,
) -> anyhow::Result<()> {
    let command = program
        .command(name)
        .ok_or_else(|| anyhow!("unknown command `{name}`"))?;
    bind_args(command, Defs::of(program), input).map_err(|why| anyhow!("{why}"))?;
    Ok(())
}

/// Run one command to a settled outcome, retrying a conflict per `retry`.
///
/// The budget and the wait are decided here and the attempts themselves happen inside
/// heklang, which is the only place that can make a retry cheap: it keeps the state the
/// last attempt folded and reads strictly after the position that state covers, so being
/// beaten to the log costs the events that beat you rather than the whole boundary again.
#[allow(clippy::too_many_arguments)]
pub fn run_command(
    store: &WriteHandle,
    program: &Arc<Program>,
    events: &Arc<EventDefs>,
    name: &str,
    keystore: Option<&Arc<KeyStore>>,
    input: &serde_json::Value,
    ctx: &CommandContext,
    now: &str,
    idem_tag: Option<&str>,
    retry: &Retry<'_>,
) -> anyhow::Result<CommandOutcome> {
    let command = program
        .command(name)
        .ok_or_else(|| anyhow!("unknown command `{name}`"))?;
    let args = match bind_args(command, Defs::of(program), input) {
        Ok(args) => args,
        Err(message) => return Ok(CommandOutcome::InvalidInput { message }),
    };
    // A command with no `state` reads nothing, so it can be beaten to the log by
    // nobody: only a boundaried one can recover a duplicate commit.
    let boundaried = !command.slices.is_empty();

    if retry.max_attempts == 0 {
        return Ok(CommandOutcome::Conflict);
    }

    // One world for the whole request, not one per attempt. Every field it accumulates
    // is written by an append that ends the request: a commit stops the loop, and the
    // two out-of-band flags below leave as errors that are not conflicts.
    let host = HeklaHost {
        program: Arc::clone(program),
        events: Arc::clone(events),
        store: store.clone(),
        keystore: keystore.cloned(),
        ctx: *ctx,
        now: now.to_owned(),
        idem_tag: idem_tag.map(str::to_owned),
        // Only an effect's `invoke` keys an append on a journaled call.
        call: None,
        appended: None,
        emitted: Vec::new(),
        unavailable: None,
        duplicated: false,
        retry_after: None,
        last_transport: None,
        minted: None,
        // A command never reaches `http.*`: heklang's parser is what guarantees it,
        // so there is nothing to give one here.
        http: None,
    };
    let mut interpreter = Interpreter::with_host(program, host);
    let attempts = retry.max_attempts;
    let execution = match interpreter.run_retrying(name, args, &mut |attempt| {
        if attempt + 1 >= attempts {
            return false;
        }
        (retry.backoff)(attempt);
        true
    }) {
        Ok(execution) => execution,
        Err(err) => {
            let host = interpreter.host();
            if let Some(message) = host.unavailable.clone() {
                return Ok(CommandOutcome::Unavailable { message });
            }
            // The existence clause fired: this request committed already, so the
            // outcome is recovered rather than re-decided. A boundary is not needed
            // for this one, unlike the re-read below: the append itself caught it.
            if host.duplicated {
                let tag = idem_tag.expect("the existence clause is only set when keyed");
                return match find_committed_outcome(store, events, tag)? {
                    Some(recovered) => Ok(CommandOutcome::AlreadyCommitted(recovered)),
                    None => Err(anyhow!(
                        "the idempotency guard fired but no committed outcome was found"
                    )),
                };
            }
            // The budget ran out with the boundary still moving under it.
            if matches!(err.kind, ErrorKind::Conflict { .. }) {
                return Ok(CommandOutcome::Conflict);
            }
            return Err(anyhow!("{err}"));
        }
    };

    Ok(match execution.outcome {
        Outcome::Ok(_) => {
            let host = interpreter.host();
            // Committed, but with nothing to append. That decision can be spurious
            // under a same-key duplicate: this attempt folded the duplicate's
            // just-committed events and concluded the work was already done. No
            // append means the existence clause never fired, so the tag re-read is
            // the only thing that can catch it.
            if host.appended.is_none()
                && let Some(recovered) = recover_if_committed(store, events, boundaried, idem_tag)?
            {
                CommandOutcome::AlreadyCommitted(recovered)
            } else {
                CommandOutcome::Committed {
                    events: host.emitted.clone(),
                    positions: host.appended,
                }
            }
        }
        // Neither of these appended, so the append's own clause cannot catch a
        // same-key duplicate that committed while this attempt was folding. The
        // tag re-read is what catches it, and only a boundaried command can have
        // folded the duplicate's events in the first place.
        Outcome::Reject { code, message } => {
            match recover_if_committed(store, events, boundaried, idem_tag)? {
                Some(recovered) => CommandOutcome::AlreadyCommitted(recovered),
                None => CommandOutcome::Rejected { code, message },
            }
        }
        Outcome::Invalid(message) => {
            match recover_if_committed(store, events, boundaried, idem_tag)? {
                Some(recovered) => CommandOutcome::AlreadyCommitted(recovered),
                None => CommandOutcome::InvalidInput { message },
            }
        }
    })
}

/// A keyed request's own prior commit, checked when this attempt is about to return
/// without appending anything.
fn recover_if_committed(
    store: &WriteHandle,
    event_defs: &EventDefs,
    boundaried: bool,
    idem_tag: Option<&str>,
) -> anyhow::Result<Option<RecoveredOutcome>> {
    match (boundaried, idem_tag) {
        (true, Some(tag)) => find_committed_outcome(store, event_defs, tag),
        _ => Ok(None),
    }
}

/// A query item matching any event carrying the idempotency tag.
fn idem_item(tag: &str) -> anyhow::Result<QueryItem> {
    let tag = Tag::new(tag.to_owned())
        .map_err(|err| anyhow::anyhow!("invalid idempotency tag `{tag}`: {err}"))?;
    let tags =
        Tags::new([tag]).map_err(|err| anyhow::anyhow!("invalid idempotency tag set: {err}"))?;
    Ok(QueryItem::with_tags(tags))
}

/// Look for a prior committed attempt of this request in the log, by its idempotency
/// tag, so a replay returns the original outcome without re-running `handle`. Returns
/// `None` when the request has not committed yet. The read is an indexed existence
/// check on a unique, high-cardinality tag at `after = 0`: a term-dictionary probe per
/// segment, no posting-list decode (the single position inlines into the FST value).
fn find_committed_outcome(
    store: &WriteHandle,
    event_defs: &EventDefs,
    idem_tag: &str,
) -> anyhow::Result<Option<RecoveredOutcome>> {
    let query = Query::item(idem_item(idem_tag)?);
    let mut reads = store.read(&query, Position::ZERO, None);
    let mut events = Vec::new();
    let mut range: Option<(Position, Position)> = None;
    let mut ids: Option<(Uuid, Uuid)> = None;
    while let Some(item) = reads.next() {
        let seq = item.map_err(|err| anyhow::anyhow!("reading idempotent replay: {err}"))?;
        range = Some(match range {
            Some((first, _)) => (first, seq.position),
            None => (seq.position, seq.position),
        });
        let (envelope, _data) = envelope::decode(seq.event.data())
            .map_err(|err| anyhow::anyhow!("reading event: {err}"))?;
        match ids {
            None => ids = Some((envelope.correlation_id, envelope.causation_id)),
            // Every event of one command execution shares its causation id. A second
            // distinct one means two logical requests matched this tag (a double
            // commit, or an astronomically unlikely hash collision): surface it rather
            // than splice both commits into one bogus recovered outcome.
            Some((_, causation)) if envelope.causation_id != causation => {
                anyhow::bail!("idempotency tag matches events from more than one command execution")
            }
            Some(_) => {}
        }
        // Report the same tags a fresh commit does: plaintext, non-subject indexed
        // fields only. Subject fields are stored as ciphertext (and the recovery path
        // deliberately cannot decrypt), and the reserved host tags (`_hekla_idem`, the
        // `_hekla_uniq_` global tags) are internal, so both are dropped.
        let def = event_defs.get(seq.event.event_type());
        let mut tags: Vec<String> = seq
            .event
            .tags()
            .filter(|tag| !tag.starts_with(RESERVED_TAG_PREFIX))
            .filter(|tag| {
                let key = tag.split(':').next().unwrap_or(tag);
                !def.is_some_and(|def| def.is_subject(key))
            })
            .map(str::to_owned)
            .collect();
        // Stored tag sets are sorted; sorting the response tags too keeps recovery
        // byte-identical to the live outcome, which also sorts (see `tag_strings`).
        tags.sort();
        events.push(RecoveredEvent {
            event_type: seq.event.event_type().to_owned(),
            tags,
        });
    }
    let Some((first, last)) = range else {
        return Ok(None);
    };
    let (correlation_id, causation_id) = ids.expect("a matched event carries an envelope");
    Ok(Some(RecoveredOutcome {
        events,
        positions: PositionRange { first, last },
        correlation_id,
        causation_id,
    }))
}
