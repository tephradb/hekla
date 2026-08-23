//! Dispatch fails closed on an event type the registry does not know.
//!
//! The loader rejects a module-level `event(...)` outside `events/`, but a
//! definition built inside a function body slips past that scan: it is never
//! registered, so `lower_event` has no schema for it. Without the guard the event
//! was appended anyway, writing a `subject` field to the immutable log as plaintext
//! in both the payload and its tag, where it can never be erased, and silently.

use serde_json::json;

mod support;

use support::{Boot, ctx, log_head, write_project};

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
