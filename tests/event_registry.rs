//! An event type is a declaration, and a handler can only name one.
//!
//! The Starlark suite had three tests here, and all three described defences that no
//! longer have anything to defend against. A handler could call `event(...)` and emit
//! the result, so a forged schema could put a `subject` field in the log as plaintext;
//! it could redeclare a type under a second definition; and it could reach a type
//! through an alias. In heklang an `event` is a top-level declaration, `emit` names a
//! path rather than a value, and there is no `load` to alias through, so none of those
//! is expressible. What is left is the pair of parse-time rules underneath them.

mod support;

use support::{assert_clean, assert_error};

const EVENTS: &str = r#"
event @order.placed {
  order_id: Uuid,
  customer_id: Int,
  email: String? @subject(customer_id) @max(100),
}
"#;

#[test]
fn a_command_cannot_emit_a_type_that_is_not_declared() {
    assert_error(
        &[
            ("events/order.hk", EVENTS),
            (
                "commands/sneak.hk",
                r#"
command Sneak(owner: Int, secret: String) {
  emit @thing.sneaked { owner, secret }
}
"#,
            ),
        ],
        "@thing.sneaked",
    );
}

#[test]
fn one_event_type_is_declared_once() {
    assert_error(
        &[
            ("events/order.hk", EVENTS),
            (
                "events/again.hk",
                "event @order.placed { order_id: Uuid, customer_id: Int }\n",
            ),
        ],
        "order.placed",
    );
}

#[test]
fn a_declared_type_emits_from_any_command() {
    assert_clean(&[
        ("events/order.hk", EVENTS),
        (
            "commands/place.hk",
            r#"
command Place(order_id: Uuid, customer_id: Int, email: String?) {
  emit @order.placed { order_id, customer_id, email }
}
"#,
        ),
    ]);
}
