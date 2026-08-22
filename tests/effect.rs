//! Effect durability, end to end. The `send-welcome` effect fires on registration:
//! its HTTP call is journaled and it invokes the internal `record-welcome`
//! command, which appends `user.welcomed`. These tests pin the durable properties:
//! the invocation runs once, restarting the runtime replays the journal without
//! re-firing a completed invocation, and a 5xx wedges the effect (visible in the
//! health signals) until an explicit operator skip advances it.

use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use kiln::context::CommandContext;
use kiln::effect::{EffectRuntime, HttpClient, StubHttpClient};
use kiln::loader::LoadedProject;
use kiln::projector::ProjectorSet;
use kiln::runtime::Runtime;
use serde_json::json;
use tephra::WriteCoordinator;
use uuid::Uuid;

const ALICE: &str = "11111111-1111-1111-1111-111111111111";
const EFFECT: &str = "send-welcome";

struct Booted {
    rt: Arc<Runtime>,
    coord: WriteCoordinator,
    projectors: ProjectorSet,
    effects: EffectRuntime,
}

impl Booted {
    fn shutdown(self) {
        self.effects.shutdown_and_join();
        self.projectors.shutdown_and_join();
        self.coord.shutdown();
    }
}

fn boot(data: &Path, http: Arc<dyn HttpClient>) -> Booted {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/users");
    let project = LoadedProject::load(&root);
    assert!(!project.has_errors(), "{:?}", project.findings);
    let (rt, coord, projectors, effects) = Runtime::open(project, data, http, None).unwrap();
    Booted {
        rt,
        coord,
        projectors,
        effects,
    }
}

fn register(rt: &Runtime, id: &str) {
    let ctx = CommandContext::new(Uuid::new_v4());
    let body = json!({ "user_id": id, "email": format!("{id}@example.com"), "name": "U" });
    assert_eq!(
        rt.execute("register-user", body, &ctx, None)
            .unwrap()
            .status,
        200
    );
}

fn wait_until<F: Fn() -> bool>(label: &str, cond: F) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {label}");
}

fn log_head(rt: &Runtime) -> u64 {
    rt.status()["log_head"].as_u64().unwrap()
}

fn effect_position(rt: &Runtime) -> u64 {
    rt.effect(EFFECT).unwrap().position()
}

#[test]
fn effect_fires_a_journaled_http_call_then_invokes_the_command_once() {
    let data = tempfile::tempdir().unwrap();
    let stub = Arc::new(StubHttpClient::ok());
    let booted = boot(data.path(), stub.clone());

    register(&booted.rt, ALICE);
    // The effect posts the welcome, then invokes record-welcome, which appends
    // user.welcomed: the log head reaches 2.
    wait_until("effect to complete", || log_head(&booted.rt) >= 2);

    // Exactly one POST, to the welcome URL, carrying the registered email.
    assert_eq!(stub.call_count(), 1);
    let call = &stub.calls()[0];
    assert_eq!(call.method, "POST");
    assert_eq!(call.url, "https://example.test/welcome");
    let body: serde_json::Value =
        serde_json::from_slice(call.body.as_deref().expect("a POST body")).unwrap();
    assert_eq!(body["email"], format!("{ALICE}@example.com"));

    // record-welcome landed exactly once: the head is 2, not 3, and stays there.
    thread::sleep(Duration::from_millis(50));
    assert_eq!(log_head(&booted.rt), 2);

    booted.shutdown();
}

#[test]
fn restarting_replays_the_journal_without_refiring() {
    let data = tempfile::tempdir().unwrap();

    // First boot: process the registration to completion.
    let stub1 = Arc::new(StubHttpClient::ok());
    let booted = boot(data.path(), stub1.clone());
    register(&booted.rt, ALICE);
    // Wait until the effect has advanced past both the registration and the
    // user.welcomed it produced, so the invocation is terminal on disk.
    wait_until("first run to settle", || effect_position(&booted.rt) >= 2);
    assert_eq!(stub1.call_count(), 1);
    booted.shutdown();

    // Second boot on the same data directory: the invocation is terminal, so the
    // effect must not re-enter handle. No new HTTP call, no duplicate event.
    let stub2 = Arc::new(StubHttpClient::ok());
    let booted = boot(data.path(), stub2.clone());
    wait_until("effect to catch up", || effect_position(&booted.rt) >= 2);
    thread::sleep(Duration::from_millis(50));
    assert_eq!(
        stub2.call_count(),
        0,
        "a completed invocation must not re-fire its http call"
    );
    assert_eq!(log_head(&booted.rt), 2, "no duplicate user.welcomed");

    booted.shutdown();
}

#[test]
fn invoke_commands_boundary_dedupes_a_replay_when_the_key_is_lost() {
    // Simulates the append-then-finalize crash window: on restart the idempotency
    // key is cleared, so a replay re-invokes and reserve() re-acquires rather than
    // replaying the stored outcome. record-welcome carries a DCB boundary, so the
    // second append is a no-op reject, not a duplicate event. Two distinct keys
    // stand in for the cleared-then-re-acquired key.
    let data = tempfile::tempdir().unwrap();
    let booted = boot(data.path(), Arc::new(StubHttpClient::status(400)));

    let ctx = CommandContext::from_effect(Uuid::new_v4(), Uuid::new_v4());
    let input = json!({ "user_id": ALICE });
    let first = booted
        .rt
        .execute_from_effect("record-welcome", input.clone(), &ctx, Some("key-a"))
        .unwrap();
    assert_eq!(first.status, 200);

    let second = booted
        .rt
        .execute_from_effect("record-welcome", input, &ctx, Some("key-b"))
        .unwrap();
    assert_eq!(second.status, 422);
    assert_eq!(second.body["error"]["code"], "already_welcomed");
    assert_eq!(
        log_head(&booted.rt),
        1,
        "the boundary appended user.welcomed once"
    );

    booted.shutdown();
}

#[test]
fn a_5xx_wedges_the_effect_and_an_operator_skip_advances_it() {
    let data = tempfile::tempdir().unwrap();
    // A persistent 5xx is absorbed by the runtime and never reaches the script, so
    // the effect wedges rather than skipping.
    let stub = Arc::new(StubHttpClient::status(500));
    let booted = boot(data.path(), stub.clone());

    register(&booted.rt, ALICE); // user.registered at position 1

    wait_until("the wedge to surface in status", || {
        booted.rt.effect(EFFECT).unwrap().consecutive_failures() > 0
    });
    let effect = booted.rt.effect(EFFECT).unwrap();
    assert!(
        effect.last_error().is_some(),
        "a wedge records its last error"
    );
    assert_eq!(log_head(&booted.rt), 1, "a wedged effect appends nothing");

    // An explicit, manual operator skip advances past the unprocessable event.
    effect.request_skip(1);
    wait_until("the skip to advance the effect", || {
        let effect = booted.rt.effect(EFFECT).unwrap();
        effect.consecutive_failures() == 0 && effect.position() >= 1
    });
    assert_eq!(log_head(&booted.rt), 1, "skipping does not append");

    booted.shutdown();
}
