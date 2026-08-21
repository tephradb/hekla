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
}

/// Host context passed to a command's `handle` via `eval.extra`. Present only for
/// the `handle` call, so `now()` resolves there and errors in `query` and `fold`.
#[derive(Debug, ProvidesStaticType)]
pub struct HandleCtx {
    /// The request's pinned append time, RFC 3339.
    pub now: String,
}

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
