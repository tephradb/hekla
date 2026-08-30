//! The reserved tag namespace hekla stamps onto every event.
//!
//! These are the host's bookkeeping, not the language's: heklang knows nothing about
//! them, and a program can neither emit one nor fold over one. Keeping them in their
//! own module is what makes that boundary checkable rather than remembered.

use uuid::Uuid;

use crate::hash;

/// The reserved tag-key prefix hekla stamps onto events for host bookkeeping. An event
/// field can never occupy this namespace, so a program cannot forge a host tag or an
/// append condition.
pub const RESERVED_TAG_PREFIX: &str = "_hekla_";

/// The reserved tag key carrying a command's per-request idempotency identity. Every
/// event a keyed command emits gets this tag and the append is guarded against it, so
/// exactly-once is enforced by the log itself rather than by op-DB bookkeeping.
const IDEMPOTENCY_TAG_KEY: &str = "_hekla_idem";

/// The reserved tag key carrying the correlation id of the flow an event belongs to.
/// Every event gets one, which is what makes a causal chain an indexed tag probe rather
/// than a scan with an envelope decode per event.
const CORRELATION_TAG_KEY: &str = "_hekla_corr";

/// The idempotency tag for a `(command, key)` pair.
///
/// Hashing binds the tag to the command, so the same key on two commands cannot
/// collide, and yields a fixed-length value whatever the client's raw key was. The
/// request body is deliberately excluded: the key alone identifies the request, so
/// reusing a key with a different body replays the first outcome rather than running
/// the new one.
pub fn idempotency_tag(command: &str, key: &str) -> String {
    let mut material = Vec::with_capacity(command.len() + 1 + key.len());
    material.extend_from_slice(command.as_bytes());
    material.push(0);
    material.extend_from_slice(key.as_bytes());
    format!("{IDEMPOTENCY_TAG_KEY}:{}", hash::sha256_hex(&material))
}

/// The correlation tag for one flow.
///
/// Unlike the idempotency tag this is not hashed. The value is already a uuid, so it is
/// fixed-length and fixed-charset, and leaving it readable means an operator can take a
/// `correlation_id` out of a command response and query for it directly.
pub fn correlation_tag(correlation_id: Uuid) -> String {
    // Rendered into a stack buffer rather than through `to_string`, so stamping the tag
    // on each of a command's events costs one allocation per event and not two.
    let mut buf = Uuid::encode_buffer();
    correlation_tag_value(correlation_id.hyphenated().encode_lower(&mut buf))
}

/// The correlation tag for an already-rendered id, so a request that received one as a
/// path segment does not have to round-trip it through [`Uuid`] to query for it.
pub fn correlation_tag_value(correlation_id: &str) -> String {
    format!("{CORRELATION_TAG_KEY}:{correlation_id}")
}
