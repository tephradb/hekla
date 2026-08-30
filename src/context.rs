//! The causation metadata one command execution carries.
//!
//! This is hekla's and not the language's: heklang has no opinion on why an event was
//! written, only on what it says. Nothing else lives here, because heklang gates a
//! capability by the kind of declaration it is in rather than by what happens to be
//! attached to an evaluator.

use uuid::Uuid;

/// The causation metadata for one command execution.
#[derive(Debug, Clone, Copy)]
pub struct CommandContext {
    pub correlation_id: Uuid,
    pub causation_id: Uuid,
    pub triggering_event_id: Option<Uuid>,
}

impl CommandContext {
    /// A context for a request in `correlation_id`'s flow, with a fresh causation id
    /// and no triggering event (the HTTP entry point).
    pub fn new(correlation_id: Uuid) -> CommandContext {
        CommandContext {
            correlation_id,
            causation_id: Uuid::new_v4(),
            triggering_event_id: None,
        }
    }

    /// A context for a command invoked by an effect: it keeps the flow's
    /// `correlation_id` and records the event that triggered the effect as the causing
    /// event, with a fresh causation id for this execution.
    pub fn from_effect(correlation_id: Uuid, triggering_event_id: Uuid) -> CommandContext {
        CommandContext {
            correlation_id,
            causation_id: Uuid::new_v4(),
            triggering_event_id: Some(triggering_event_id),
        }
    }
}
