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

use std::fmt;

use serde::de::{self, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Per-event metadata stamped by the host at append. `data` is stored alongside
/// these fields but kept out of this struct so [`decode`] can hand the payload
/// back separately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
    ///
    /// Serialize is derived; Deserialize is not, and deliberately so: reading goes
    /// through [`StoredVisitor`], which builds this struct field by field, so adding a
    /// field here fails to compile until the visitor reads it. A derived `Deserialize`
    /// sitting unused beside a hand-written reader is how the two sides drift.
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

struct Stored {
    envelope: Envelope,
    data: Value,
}

/// Deserialised by hand rather than with `#[serde(flatten)]`, which is the mirror of
/// [`StoredRef`] but not its equal in cost: on the deserialize side flatten buffers the
/// whole event into an intermediate map and then deserialises a second time out of it.
/// Every store read pays that, and a fold over a deep boundary pays it per event, so
/// the one place it is worth writing a visitor is here.
///
/// Unknown keys are skipped rather than rejected, matching what flatten did and what
/// forward compatibility needs: an envelope field added by a later version must not
/// make older events unreadable.
impl<'de> Deserialize<'de> for Stored {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Stored, D::Error> {
        deserializer.deserialize_map(StoredVisitor)
    }
}

/// Which key a map entry names. Deserialised through its own visitor rather than as a
/// string, because every string form allocates: serde's `Cow` impl always takes the
/// owned branch (`#[serde(borrow)]` works only in derive), and a borrowed `&str` fails
/// on any key carrying an escape. Matching on `visit_str` allocates nothing at all,
/// which is the whole point of hand-writing this on a path every store read takes.
enum Field {
    EventId,
    Timestamp,
    CorrelationId,
    CausationId,
    TriggeringEventId,
    Data,
    Unknown,
}

impl<'de> Deserialize<'de> for Field {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Field, D::Error> {
        deserializer.deserialize_identifier(FieldVisitor)
    }
}

struct FieldVisitor;

impl Visitor<'_> for FieldVisitor {
    type Value = Field;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an event envelope field name")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Field, E> {
        Ok(match value {
            "event_id" => Field::EventId,
            "timestamp" => Field::Timestamp,
            "correlation_id" => Field::CorrelationId,
            "causation_id" => Field::CausationId,
            "triggering_event_id" => Field::TriggeringEventId,
            "data" => Field::Data,
            _ => Field::Unknown,
        })
    }
}

struct StoredVisitor;

impl<'de> Visitor<'de> for StoredVisitor {
    type Value = Stored;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a stored event envelope")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Stored, M::Error> {
        let mut event_id = None;
        let mut timestamp = None;
        let mut correlation_id = None;
        let mut causation_id = None;
        let mut triggering_event_id = None;
        let mut data = None;
        // A repeated key is refused rather than last-one-wins, matching what the
        // derive did: this is the one seam every store read passes through, so bytes
        // that decode two ways should decode neither.
        macro_rules! once {
            ($slot:ident, $name:literal) => {{
                if $slot.is_some() {
                    return Err(de::Error::duplicate_field($name));
                }
                $slot = Some(map.next_value()?);
            }};
        }
        while let Some(key) = map.next_key::<Field>()? {
            match key {
                Field::EventId => once!(event_id, "event_id"),
                Field::Timestamp => once!(timestamp, "timestamp"),
                Field::CorrelationId => once!(correlation_id, "correlation_id"),
                Field::CausationId => once!(causation_id, "causation_id"),
                Field::Data => once!(data, "data"),
                // Optional, so presence is the value rather than the slot: a second
                // occurrence is still refused, including a second explicit null.
                Field::TriggeringEventId => {
                    if triggering_event_id.is_some() {
                        return Err(de::Error::duplicate_field("triggering_event_id"));
                    }
                    triggering_event_id = Some(map.next_value::<Option<Uuid>>()?);
                }
                Field::Unknown => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        let triggering_event_id = triggering_event_id.flatten();
        Ok(Stored {
            envelope: Envelope {
                event_id: event_id.ok_or_else(|| de::Error::missing_field("event_id"))?,
                timestamp: timestamp.ok_or_else(|| de::Error::missing_field("timestamp"))?,
                correlation_id: correlation_id
                    .ok_or_else(|| de::Error::missing_field("correlation_id"))?,
                causation_id: causation_id
                    .ok_or_else(|| de::Error::missing_field("causation_id"))?,
                triggering_event_id,
            },
            data: data.ok_or_else(|| de::Error::missing_field("data"))?,
        })
    }
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

    /// What a later version adding an envelope field would look like to this one.
    /// Skipping the key rather than failing is what keeps old readers working.
    #[test]
    fn decode_ignores_an_unknown_envelope_field() {
        let bytes = encode(&sample(), &serde_json::json!({"n": 1})).unwrap();
        let mut stored: serde_json::Map<String, Value> = serde_json::from_slice(&bytes).unwrap();
        stored.insert("shard".to_owned(), serde_json::json!({"a": [1, 2]}));
        let bytes = serde_json::to_vec(&stored).unwrap();
        let (back, back_data) = decode(&bytes).unwrap();
        assert_eq!(back, sample());
        assert_eq!(back_data, serde_json::json!({"n": 1}));
    }

    #[test]
    fn decode_names_a_missing_envelope_field() {
        let bytes = encode(&sample(), &serde_json::json!({})).unwrap();
        let mut stored: serde_json::Map<String, Value> = serde_json::from_slice(&bytes).unwrap();
        stored.remove("causation_id");
        let bytes = serde_json::to_vec(&stored).unwrap();
        let err = decode(&bytes).unwrap_err().to_string();
        assert!(err.contains("causation_id"), "{err}");
    }

    #[test]
    fn decode_names_a_missing_payload() {
        let bytes = encode(&sample(), &serde_json::json!({})).unwrap();
        let mut stored: serde_json::Map<String, Value> = serde_json::from_slice(&bytes).unwrap();
        stored.remove("data");
        let bytes = serde_json::to_vec(&stored).unwrap();
        let err = decode(&bytes).unwrap_err().to_string();
        assert!(err.contains("data"), "{err}");
    }

    /// Bytes that decode two ways should decode neither. `decode` is the single
    /// integrity seam every store read passes through, so a repeated key (a tampered
    /// or double-encoded event) is refused rather than resolved last-one-wins.
    #[test]
    fn decode_refuses_a_repeated_field() {
        let one = Uuid::from_u128(7);
        let two = Uuid::from_u128(8);
        for (field, extra) in [
            ("event_id", format!("\"event_id\":\"{two}\"")),
            ("data", "\"data\":{\"b\":2}".to_owned()),
            (
                "triggering_event_id",
                format!("\"triggering_event_id\":\"{two}\""),
            ),
        ] {
            let bytes = format!(
                "{{\"event_id\":\"{one}\",\"timestamp\":\"t\",\"correlation_id\":\"{one}\",\
                 \"causation_id\":\"{one}\",\"triggering_event_id\":\"{one}\",\
                 \"data\":{{\"a\":1}},{extra}}}"
            );
            let err = decode(bytes.as_bytes()).unwrap_err().to_string();
            // Matched with the backticks serde puts around the name, because
            // `event_id` is a substring of `triggering_event_id` and the payload
            // carries both: a bare `contains` would pass on the wrong error.
            assert!(
                err.contains(&format!("`{field}`")),
                "duplicate {field} accepted, or blamed on another field: {err}"
            );
        }
    }

    /// The payload is whatever the event declared, so decoding must not assume an
    /// object: a scalar or a list round-trips as itself.
    #[test]
    fn decode_accepts_any_payload_shape() {
        for data in [
            serde_json::json!([1, 2, 3]),
            serde_json::json!("text"),
            serde_json::json!(null),
        ] {
            let bytes = encode(&sample(), &data).unwrap();
            let (_, back) = decode(&bytes).unwrap();
            assert_eq!(back, data);
        }
    }
}
