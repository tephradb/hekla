//! Per-request command context and the host handle context.
//!
//! [`CommandContext`] carries the causation metadata a command stamps onto the
//! events it emits: the correlation id (the flow this request belongs to), a
//! fresh causation id (this command execution), and an optional triggering event.
//! [`HandleCtx`] is the host context a command's `handle` reads through
//! `eval.extra`, holding the request's pinned `now()` so repeated calls agree and
//! so `query` and `fold`, which run without it, cannot reach a clock.

use starlark::any::ProvidesStaticType;
use uuid::Uuid;

/// The causation metadata for one command execution.
#[derive(Debug, Clone, Copy)]
pub struct CommandContext {
    pub correlation_id: Uuid,
    pub causation_id: Uuid,
    pub triggering_event_id: Option<Uuid>,
}

impl CommandContext {
    /// A context for a request in `correlation_id`'s flow, with a fresh causation
    /// id and no triggering event (the HTTP entry point).
    pub fn new(correlation_id: Uuid) -> CommandContext {
        CommandContext {
            correlation_id,
            causation_id: Uuid::new_v4(),
            triggering_event_id: None,
        }
    }

    /// A context for a command invoked by an effect: it keeps the flow's
    /// `correlation_id` and records the event that triggered the effect as the
    /// causing event, with a fresh causation id for this execution.
    pub fn from_effect(correlation_id: Uuid, triggering_event_id: Uuid) -> CommandContext {
        CommandContext {
            correlation_id,
            causation_id: Uuid::new_v4(),
            triggering_event_id: Some(triggering_event_id),
        }
    }
}

/// Host context passed to a command's `handle` via `eval.extra`. Present only for
/// the `handle` call, so `now()` resolves there and errors in `query` and `fold`.
#[derive(Debug, ProvidesStaticType)]
pub struct HandleCtx {
    /// The request's pinned append time, RFC 3339.
    pub now: String,
}

/// Marker set on the evaluator while a command's `query` runs and while a
/// projector's or effect's `source` is evaluated at load. Its presence tells an
/// event-definition call it is being used as a query filter (a subset match) rather
/// than to construct an event to emit (which requires every field). It is distinct
/// from [`HandleCtx`], so `now()` still errors in `query`.
#[derive(ProvidesStaticType)]
pub struct QueryCtx;

/// Read access a projector's `handle` has to its own read model, through the
/// current batch's uncommitted writes. The storage-backed implementation lives in
/// the runtime; the trait sits here so the builtins layer stays independent of it.
pub trait EntityReader {
    /// The current row for `entity_id`'s entity, keyed by `key`, as a JSON object,
    /// or `None`. Reflects writes from earlier events in the same batch.
    fn get(&self, entity_id: u64, key: &str) -> anyhow::Result<Option<serde_json::Value>>;
}

/// Host context passed to a projector's `handle` via `eval.extra`, giving `get()`
/// a reader over the read model. Present only for the `handle` call, so `get()`
/// resolves there and errors anywhere a projector context is absent.
#[derive(ProvidesStaticType)]
pub struct ProjectorCtx<'a> {
    pub reader: &'a dyn EntityReader,
}

/// The impure capabilities an effect's `handle` reaches through its builtins. The
/// runtime's implementation journals each call so a replay after a crash returns
/// the recorded result instead of performing the side effect again. The trait
/// sits here so the builtins layer stays independent of the runtime that backs it.
///
/// Every method but [`log`](EffectHost::log) is journaled; `log` writes a trace
/// line and may repeat on replay, which is why it returns nothing that could
/// steer control flow.
pub trait EffectHost {
    /// Perform an HTTP request and return `{status, body, headers}`. A transport
    /// failure or a 5xx is an `Err` (the runtime retries it, so it never reaches
    /// the script); a 2xx/3xx/4xx response is a value the handler decides on.
    fn http(
        &self,
        method: &str,
        url: &str,
        headers: Vec<(String, String)>,
        body: Option<serde_json::Value>,
    ) -> anyhow::Result<serde_json::Value>;

    /// Invoke a public or internal command and return its `{status, body}`
    /// outcome. Exactly-once across replays via a deterministic idempotency key.
    fn invoke_command(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value>;

    /// Read one row from a projector's read model by key (`null` when absent).
    fn read(&self, projector: &str, entity: &str, key: &str) -> anyhow::Result<serde_json::Value>;

    /// Scan a projector's read model with an optional single indexed filter and
    /// cursor pagination, returning `{items, next_cursor}`.
    fn scan(
        &self,
        projector: &str,
        entity: &str,
        filter: Option<(String, String)>,
        cursor: Option<String>,
        limit: Option<usize>,
    ) -> anyhow::Result<serde_json::Value>;

    /// The wall clock at first run, recorded so a replay agrees, RFC 3339.
    fn now(&self) -> anyhow::Result<String>;

    /// Emit a trace line. Not journaled.
    fn log(&self, message: &str);

    /// Decrypt a subject-encrypted handle to its plaintext, the explicit boundary an
    /// effect crosses to act on personal data (e.g. to send mail). Deterministic, so
    /// it is re-run rather than cached on replay: if the subject was erased in the
    /// meantime this fails, and the failure is terminal (the data is gone; no retry
    /// can recover it). Every call is logged.
    ///
    /// Call `reveal()` before any journaled side effect (an `http` call,
    /// `invoke_command`): a terminal `reveal` completes the invocation, so a side
    /// effect that already fired stays done while the reveal-dependent work does not
    /// run. Revealing first means an erased subject aborts the handler before any
    /// external action.
    fn reveal(
        &self,
        subject_field: &str,
        subject_value: &str,
        field: &str,
        ciphertext: &str,
    ) -> anyhow::Result<String>;
}

/// Host context passed to an effect's `handle` via `eval.extra`. Present only for
/// the `handle` call, so the impure builtins resolve there and error anywhere an
/// effect context is absent (keeping commands and projectors pure).
#[derive(ProvidesStaticType)]
pub struct EffectCtx<'a> {
    pub host: &'a dyn EffectHost,
}
