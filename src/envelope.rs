//! The host-stamped event envelope.
//!
//! tephra stores opaque payload bytes and no metadata, so hekla wraps each event's
//! data in a JSON envelope carrying the identity and causation the runtime stamps
//! at append: a fresh event id, the append timestamp, and the correlation and
//! causation ids. The idempotency key is deliberately absent (it is request
//! plumbing, held in the operational DB, not domain identity on an event).
//!
//! This is the single place event data is wrapped and unwrapped. Appends encode
//! through [`encode`]; every store read decodes through [`decode`] to recover the
//! payload the handlers see. Keeping both on one seam means a command's `fold` and
//! a projector's `handle` can never accidentally observe the envelope in place of
//! the data.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Per-event metadata stamped by the host at append. `data` is stored alongside
/// these fields but kept out of this struct so [`decode`] can hand the payload
/// back separately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// A stable identity for this event, independent of its tephra position.
    pub event_id: Uuid,
    /// The append timestamp, RFC 3339. The same instant a `handle`'s `now()` sees.
    pub timestamp: String,
    /// The flow this event belongs to, propagated across a whole command chain.
    pub correlation_id: Uuid,
    /// The command execution that produced this event.
    pub causation_id: Uuid,
    /// The event that triggered the producing command, when one did (effects in a
    /// later phase). Absent for commands invoked directly over HTTP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triggering_event_id: Option<Uuid>,
}

/// The on-disk shape: the envelope fields flattened next to the payload.
#[derive(Serialize)]
struct StoredRef<'a> {
    #[serde(flatten)]
    envelope: &'a Envelope,
    data: &'a Value,
}

#[derive(Deserialize)]
struct Stored {
    #[serde(flatten)]
    envelope: Envelope,
    data: Value,
}

/// Encode an event's payload wrapped in its envelope, for the tephra payload
/// bytes. Tags are derived and stored separately by the caller and never live in
/// here.
pub fn encode(envelope: &Envelope, data: &Value) -> anyhow::Result<Vec<u8>> {
    let stored = StoredRef { envelope, data };
    serde_json::to_vec(&stored).map_err(|err| anyhow::anyhow!("encoding event envelope: {err}"))
}

/// Decode stored payload bytes into the envelope and the unwrapped `data` the
/// handlers see.
pub fn decode(bytes: &[u8]) -> anyhow::Result<(Envelope, Value)> {
    let stored: Stored =
        serde_json::from_slice(bytes).map_err(|err| anyhow::anyhow!("decoding event: {err}"))?;
    Ok((stored.envelope, stored.data))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Envelope {
        Envelope {
            event_id: Uuid::from_u128(1),
            timestamp: "2026-08-21T00:00:00Z".to_owned(),
            correlation_id: Uuid::from_u128(2),
            causation_id: Uuid::from_u128(3),
            triggering_event_id: None,
        }
    }

    #[test]
    fn round_trips_payload_and_metadata() {
        let env = sample();
        let data = serde_json::json!({"email": "a@example.com", "n": 3});
        let bytes = encode(&env, &data).unwrap();
        let (back, back_data) = decode(&bytes).unwrap();
        assert_eq!(back, env);
        assert_eq!(back_data, data);
    }

    #[test]
    fn omits_absent_triggering_event() {
        let bytes = encode(&sample(), &serde_json::json!({})).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("triggering_event_id"), "{text}");
    }

    #[test]
    fn carries_triggering_event_when_present() {
        let mut env = sample();
        env.triggering_event_id = Some(Uuid::from_u128(9));
        let bytes = encode(&env, &serde_json::json!({})).unwrap();
        let (back, _) = decode(&bytes).unwrap();
        assert_eq!(back.triggering_event_id, Some(Uuid::from_u128(9)));
    }

    #[test]
    fn decode_rejects_non_envelope_bytes() {
        assert!(decode(b"not json").is_err());
    }
}
