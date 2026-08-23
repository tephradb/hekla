//! Dispatch fails closed on an event type the registry does not know.
//!
//! The loader rejects a module-level `event(...)` outside `events/`, but a
//! definition built inside a function body slips past that scan: it is never
//! registered, so `lower_event` has no schema for it. Without the guard the event
//! was appended anyway, writing a `subject` field to the immutable log as plaintext
//! in both the payload and its tag, where it can never be erased, and silently.

use serde_json::json;

mod support;

use support::{Boot, ctx, log_head, orders_project_with, write_project};

/// The definition lives in `handle`'s body, so the loader's module-level scan never
/// sees it and the event registry never learns the type.
const SNEAK_COMMAND: &str = r#"
input = schema(owner = u64_(), secret = text())

def handle(input, state):
    sneaky = event(
        type = "thing.sneaked",
        fields = {"owner": u64_(), "secret": text(subject = "owner", max_length = 50)},
    )
    return sneaky(owner = input.owner, secret = input.secret)
"#;

#[test]
fn an_event_built_inside_handle_is_rejected_instead_of_logged_as_plaintext() {
    let dir = write_project(&[("commands/sneak.star", SNEAK_COMMAND)]);
    let harness = Boot::new(dir.path()).with_master_key().start();

    let outcome = harness.rt.execute(
        "sneak",
        json!({ "owner": 42, "secret": "alice@example.com" }),
        &ctx(),
        None,
    );
    let Err(err) = outcome else {
        panic!("an unregistered event type must fail closed, not append");
    };

    let message = format!("{err:#}");
    assert!(
        message.contains("thing.sneaked"),
        "the error names the unregistered event type: {message}"
    );
    assert_eq!(
        log_head(&harness.rt),
        0,
        "a rejected event must not reach the log"
    );

    harness.shutdown();
}

/// The redeclared definition also lives in `handle`'s body, but under a type the
/// project *does* declare. The registry has a schema for that type, so the event was
/// lowered against it and `email_plain`, which the real definition does not declare,
/// rode into the log verbatim: never validated, never encrypted, never erasable.
const SHADOW_COMMAND: &str = r#"
input = schema(order_id = uuid(), customer_id = u64_(), email = text())

def handle(input, state):
    forged = event(
        type = "order.placed",
        fields = {
            "order_id": uuid(),
            "customer_id": u64_(),
            "email": text(subject = "customer_id", max_length = 100),
            "email_plain": text(),
        },
    )
    return forged(
        order_id = input.order_id,
        customer_id = input.customer_id,
        email = input.email,
        email_plain = input.email,
    )
"#;

#[test]
fn a_declared_type_cannot_be_emitted_through_a_redeclared_definition() {
    let dir = orders_project_with(&[("commands/shadow.star", SHADOW_COMMAND)]);
    let harness = Boot::new(dir.path()).with_master_key().start();

    let outcome = harness.rt.execute(
        "shadow",
        json!({
            "order_id": "11111111-1111-1111-1111-111111111111",
            "customer_id": 42,
            "email": "alice@example.com",
        }),
        &ctx(),
        None,
    );
    let Err(err) = outcome else {
        panic!("a redeclared definition must fail closed, not append undeclared fields");
    };

    let message = format!("{err:#}");
    assert!(
        message.contains("order.placed") && message.contains("declared outside events/"),
        "the error names the type and why it was refused: {message}"
    );
    assert_eq!(
        log_head(&harness.rt),
        0,
        "a rejected event must not reach the log"
    );

    harness.shutdown();
}

/// The identity check must not catch a definition merely referred to by a second
/// name. That is the same definition, and refusing it would break a legal project.
#[test]
fn a_declared_type_still_emits_through_a_second_name() {
    let dir = orders_project_with(&[(
        "commands/aliased.star",
        r#"
load("events/order.star", "order_placed")

Placed = order_placed

input = schema(order_id = uuid(), customer_id = u64_(), email = text())

def handle(input, state):
    return Placed(
        order_id = input.order_id,
        customer_id = input.customer_id,
        email = input.email,
    )
"#,
    )]);
    let harness = Boot::new(dir.path()).with_master_key().start();

    harness
        .rt
        .execute(
            "aliased",
            json!({
                "order_id": "11111111-1111-1111-1111-111111111111",
                "customer_id": 42,
                "email": "alice@example.com",
            }),
            &ctx(),
            None,
        )
        .expect("an aliased definition is the declared one");
    assert_eq!(log_head(&harness.rt), 1);

    harness.shutdown();
}
