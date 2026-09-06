//! An effect lane: the durable name of a partition key.
//!
//! `docs/effects.md` rule 15 makes every effect arm declare a `@key`, and the values
//! those fields hold in one event name the **lane** that event is processed in. Events
//! sharing a lane are processed in log order; events in different lanes need not wait
//! for each other. That is an ordering guarantee the author chose, not a parallelism
//! hint: two warranty plans in one shop write variants onto the same remote product, so
//! they share a lane even though per-plan parallelism looks tempting.
//!
//! **A lane is the key alone.** Rule 15 wants two arms of one effect touching one remote
//! resource under one shop id to share a lane, so the arm that produced the key is not
//! part of it. (Rule 15's *collapse* grouping for `on latest` is the narrower
//! `(arm index, key)`, because two arms have two bodies. That is a different thing and
//! this module is not it.)
//!
//! [`heklang::value::Key`] has no `Display`, no serde and no `FromStr`, so the encoding
//! is hekla's. It has to be injective, because a collision would silently serialise two
//! unrelated aggregates, or worse, let one lane's watermark claim another's positions.
//! Two ways it could collide and does not:
//!
//! - **The discriminant is kept.** heklang keeps `Key::Str("7")` and `Key::Uuid("7")`
//!   distinct deliberately, so the tag character does too.
//! - **Composite order is significant.** `{ @key a, @key b }` and `{ @key b, @key a }`
//!   are different lanes, which is why heklang's digest does not sort them and why
//!   nothing here sorts them either.
//!
//! **It is not order-preserving, and must not be made so.** Nothing sorts lanes by their
//! key: they are looked up, not ranged over. Zero-padding the integers "so they sort"
//! would break negatives and buy nothing.

use std::fmt;
use std::sync::Arc;

use heklang::value::Key;

use crate::hash::sha256_hex;

/// Separates one key's encoding from the next in a composite.
const UNIT: char = '\u{1f}';
/// Separates an enum's type from its variant, inside one key's encoding.
const PART: char = '\u{1e}';

/// The longest encoding kept verbatim. A `String` key is unbounded, and a lane id is
/// held in memory per active lane, written to `effect_lane` and printed in `/status`, so
/// something has to bound it. Past this the encoding is replaced by its hash, which
/// stays injective in practice and stays *recognisable* as elided because no verbatim
/// encoding can begin with [`ELIDED`].
const LANE_MAX: usize = 512;

/// The tag on a hashed lane id. Not one of the key tags, so an elided lane can never be
/// read back as a `String` key that happened to look like a hash.
const ELIDED: &str = "h:";

/// The lane an event is processed in: its arm's `@key` values, in written order.
///
/// `Arc<str>` because a lane id is cloned into a queue, a running set and a health map
/// on every admitted position, and it is immutable once built.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LaneId(Arc<str>);

impl LaneId {
    /// The lane a position lands in when its key cannot be read at all: an event whose
    /// declaration no longer has the `@key` field, or one holding a value that cannot
    /// name a lane.
    ///
    /// Such a position must **wedge where it is** rather than be skipped, so it needs
    /// somewhere to be dispatched to; `deliver` then computes the same key, fails the
    /// same way, and reports it in heklang's own words. `!` is not a key tag, so this
    /// can never collide with a real lane. Every unreadable position shares it, which
    /// serialises positions that were all going to wedge anyway.
    pub fn unreadable() -> LaneId {
        LaneId(Arc::from("!"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for LaneId {
    /// Rebuilds a lane id read back from `effect_lane`. The encoding never round-trips
    /// to a `Vec<Key>` and does not need to: a lane is compared and displayed, never
    /// decoded.
    fn from(text: &str) -> LaneId {
        LaneId(Arc::from(text))
    }
}

/// The lane `keys` names, as [`heklang::partition_key`] produced them.
pub fn encode(keys: &[Key]) -> LaneId {
    let mut out = String::new();
    for (index, key) in keys.iter().enumerate() {
        if index > 0 {
            out.push(UNIT);
        }
        push_key(key, &mut out);
    }
    if out.len() > LANE_MAX {
        out = format!("{ELIDED}{}", sha256_hex(out.as_bytes()));
    }
    LaneId(Arc::from(out))
}

fn push_key(key: &Key, out: &mut String) {
    match key {
        Key::Int(value) => {
            out.push_str("i:");
            out.push_str(&value.to_string());
        }
        Key::Str(text) => {
            out.push_str("s:");
            escape(text, out);
        }
        Key::Uuid(text) => {
            out.push_str("u:");
            escape(text, out);
        }
        Key::Timestamp(value) => {
            out.push_str("t:");
            out.push_str(&value.to_string());
        }
        Key::Enum { ty, variant } => {
            out.push_str("e:");
            escape(ty, out);
            out.push(PART);
            escape(variant, out);
        }
    }
}

/// Makes the separators unambiguous inside free-form text. Only a `String` key and an
/// enum's names can contain them; the numeric and UUID forms cannot.
fn escape(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            UNIT => out.push_str("\\u"),
            PART => out.push_str("\\v"),
            other => out.push(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::slice;

    use super::*;
    use crate::propgen;
    use proptest::prelude::*;

    fn int(value: i64) -> Key {
        Key::Int(value)
    }

    fn text(value: &str) -> Key {
        Key::Str(Arc::from(value))
    }

    fn uuid(value: &str) -> Key {
        Key::Uuid(Arc::from(value))
    }

    #[test]
    fn the_discriminant_separates_keys_that_spell_alike() {
        assert_ne!(
            encode(&[text("7")]),
            encode(&[uuid("7")]),
            "heklang keeps these distinct, so the lane must too"
        );
        assert_ne!(encode(&[text("7")]), encode(&[int(7)]));
        assert_ne!(encode(&[int(7)]), encode(&[Key::Timestamp(7)]));
    }

    #[test]
    fn a_composite_is_a_sequence_and_not_a_set() {
        assert_ne!(
            encode(&[int(1), int(2)]),
            encode(&[int(2), int(1)]),
            "`{{ @key a, @key b }}` and `{{ @key b, @key a }}` are different lanes"
        );
    }

    /// The case a naive join gets wrong: one key holding the separator could otherwise
    /// spell a two-key composite.
    #[test]
    fn a_separator_inside_a_key_does_not_split_it() {
        assert_ne!(
            encode(&[text(&format!("a{UNIT}s:b"))]),
            encode(&[text("a"), text("b")]),
        );
        assert_ne!(
            encode(&[Key::Enum {
                ty: "T".to_owned(),
                variant: "a".to_owned(),
            }]),
            encode(&[text(&format!("T{PART}a"))]),
        );
    }

    #[test]
    fn a_backslash_is_not_an_escape_the_reader_invented() {
        assert_ne!(encode(&[text("\\u")]), encode(&[text(&UNIT.to_string())]));
    }

    #[test]
    fn an_empty_string_key_is_still_a_lane() {
        assert_eq!(encode(&[text("")]).as_str(), "s:");
        assert_ne!(encode(&[text("")]), LaneId::unreadable());
    }

    #[test]
    fn an_oversized_key_is_elided_to_a_hash_that_says_so() {
        let long = encode(&[text(&"x".repeat(LANE_MAX + 1))]);
        assert!(long.as_str().starts_with(ELIDED), "{long}");
        assert_ne!(long, encode(&[text(&"x".repeat(LANE_MAX + 2))]));
    }

    /// No verbatim encoding begins with the elision tag, so a hashed lane can never be
    /// confused with a short one.
    #[test]
    fn a_short_key_never_looks_elided() {
        for key in [text("h:abc"), uuid("h:abc"), int(-1), Key::Timestamp(0)] {
            assert!(!encode(slice::from_ref(&key)).as_str().starts_with(ELIDED));
        }
    }

    #[test]
    fn the_unreadable_lane_is_not_a_key() {
        assert_eq!(LaneId::unreadable().as_str(), "!");
        assert_ne!(LaneId::unreadable(), encode(&[]));
    }

    proptest! {
        /// The whole contract, as one property: two key sequences encode alike only if
        /// they *are* alike. A collision would silently merge two aggregates' lanes.
        #[test]
        fn encoding_is_injective(one in propgen::keys(), two in propgen::keys()) {
            prop_assert_eq!(encode(&one) == encode(&two), one == two);
        }
    }
}
