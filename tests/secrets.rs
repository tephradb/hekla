//! Deployment credentials end to end: resolution, the boot refusal, and the one
//! property the whole feature exists for.
//!
//! **The redaction test is the load-bearing one.** heklang keeps a credential out of a
//! log line, an event and a read model by construction, and builds its own journal key
//! and `Unreachable` message from the redacted rendering. What it cannot reach is the
//! transport's error text, which `ureq` writes below the seam with the url it was handed
//! in it. A webhook address *is* the credential, so without hekla substituting the
//! redaction there, the first DNS failure publishes it to `/status`, to `/admin` and to
//! the logs. That is a leak no type can catch and no reviewer would see, which is why it
//! is asserted rather than trusted.
//!
//! Every credential here is resolved from a **file inside the test's own directory**,
//! never from the process environment. `tests/master_key_env.rs` explains why anything
//! that mutates the environment has to be quarantined in its own binary; a file source
//! sidesteps that entirely and is the shape a real deployment uses anyway.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use hekla::effect::StubHttpClient;
use hekla::http::HttpRequest;
use hekla::loader::{LoadedProject, Severity};
use rusqlite::Connection;
use tempfile::TempDir;

mod support;
use support::{Boot, UUID_A, ctx, wait_until};

/// The address the effect posts to, which is also the credential. A webhook url is the
/// case the whole feature is shaped around: there is no header to hide it in.
const WEBHOOK: &str = "https://hooks.test/services/T000/B000/XXXXsupersecretXXXX";

const EVENTS: &str = r#"
event @order.placed {
  order_id: Uuid,
  customer_id: Int,
}
"#;

const PLACE_ORDER: &str = r#"
command PlaceOrder(order_id: Uuid, customer_id: Int) {
  emit @order.placed { order_id, customer_id }
}
"#;

const ALERT_EFFECT: &str = r#"
secret WEBHOOK_URL

effect Alert {
  on @order.placed as e { @key customer_id, order_id } {
    http.post(WEBHOOK_URL, { "order": order_id })
  }
}
"#;

/// A project whose effect posts to a credential, with the credential in a file beside
/// it. Both live in the returned directory, so nothing leaks between tests and the
/// environment is never touched.
fn project_with(value: &str, declaration: &str) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let write = |rel: &str, body: &str| {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    };
    write("events/order.hk", EVENTS);
    write("commands/place-order.hk", PLACE_ORDER);
    write("effects/alert.hk", declaration);
    let credential = dir.path().join("credential");
    fs::write(&credential, value).unwrap();
    write(
        "hekla.toml",
        &format!(
            "[secrets]\nWEBHOOK_URL = {{ file = \"{}\" }}\n",
            credential.display()
        ),
    );
    dir
}

fn open_op_db(data: &Path) -> Connection {
    Connection::open(data.join("hekla.db")).unwrap()
}

/// One event, to give the effect something to react to.
fn place_order(rt: &hekla::runtime::Runtime) {
    let body = serde_json::json!({ "order_id": UUID_A, "customer_id": 7 });
    let result = rt.execute("PlaceOrder", body, &ctx(), None).unwrap();
    assert_eq!(result.status, 200, "PlaceOrder failed: {:?}", result.body);
}

/// Every journal key recorded for one effect, in order. The key is a hash, so this is
/// what a rotation must not move.
fn call_hashes(db: &Connection) -> Vec<String> {
    let mut stmt = db
        .prepare("SELECT call_hash FROM effect_journal WHERE effect = 'Alert' ORDER BY rowid")
        .unwrap();
    let rows = stmt.query_map([], |row| row.get::<_, String>(0)).unwrap();
    rows.map(Result::unwrap).collect()
}

// --- the property the feature exists for ----------------------------------

#[test]
fn a_credential_in_a_url_never_reaches_an_operator_surface() {
    let project = project_with(WEBHOOK, ALERT_EFFECT);
    let data = tempfile::tempdir().unwrap();
    // The transport fails the way a real one does: `ureq`'s message carries the url it
    // was given, which is the credential. Reproduced here rather than assumed, so the
    // test fails if hekla ever stops substituting.
    let stub = Arc::new(StubHttpClient::new(|_, request: &HttpRequest| {
        anyhow::bail!("dns error: failed to lookup address for {}", request.url)
    }));
    let booted = Boot::new(project.path())
        .data_dir(data.path())
        .http(stub.clone())
        .start();

    place_order(&booted.rt);
    wait_until("the wedge to surface", || {
        booted.rt.effect("Alert").unwrap().consecutive_failures() > 0
    });

    assert!(stub.call_count() >= 1, "the transport was really attempted");

    let effect = booted.rt.effect("Alert").unwrap();
    let last_error = effect.last_error().expect("a wedge records its last error");
    assert!(
        !last_error.contains(WEBHOOK),
        "the wedge published the credential: {last_error}"
    );
    assert!(
        !last_error.contains("XXXXsupersecretXXXX"),
        "the wedge published part of the credential: {last_error}"
    );
    // Redacted, not merely absent: an operator still has to be able to tell which call
    // failed and why, or the fix is worse than the leak.
    assert!(
        last_error.contains("{SECRET:WEBHOOK_URL}"),
        "the wedge no longer names the call at all: {last_error}"
    );
    assert!(
        last_error.contains("dns error"),
        "the wedge lost the transport reason: {last_error}"
    );

    // The same string reaches `/status` and `/admin` off this field, so one assertion
    // covers all three surfaces. The whole rendered status is checked rather than the
    // one key, because a future field could carry it too.
    let status = serde_json::to_string(&booted.rt.status()).unwrap();
    assert!(
        !status.contains("XXXXsupersecretXXXX"),
        "/status published the credential: {status}"
    );

    booted.shutdown();

    // And nothing durable holds it either: journaled arguments are hashed, never
    // stored, and a failed call journals nothing at all.
    let stored = fs::read(data.path().join("hekla.db")).unwrap();
    let stored = String::from_utf8_lossy(&stored);
    assert!(
        !stored.contains("XXXXsupersecretXXXX"),
        "the operational database holds the credential"
    );
}

#[test]
fn a_rotation_does_not_move_a_journal_key() {
    // The journal key is `verb + url + body`, so a credential spelled into it would key
    // every recorded call on a string that no longer exists the moment it rotates: a
    // crash-replay would miss and re-send, and `verify` would report a divergence for
    // every historical invocation. heklang builds the key from the redaction; this is
    // the end-to-end check that hekla did not undo it.
    let hashes_for = |value: &str| {
        let project = project_with(value, ALERT_EFFECT);
        let data = tempfile::tempdir().unwrap();
        let booted = Boot::new(project.path())
            .data_dir(data.path())
            .http_status(200)
            .start();
        place_order(&booted.rt);
        wait_until("the call to be journaled", || {
            !call_hashes(&open_op_db(data.path())).is_empty()
        });
        let hashes = call_hashes(&open_op_db(data.path()));
        booted.shutdown();
        hashes
    };

    let before = hashes_for(WEBHOOK);
    let after = hashes_for("https://hooks.test/services/T111/B111/completely-different");
    assert!(!before.is_empty(), "the call was journaled at all");
    assert_eq!(
        before, after,
        "rotating a credential moved the journal key, so a replay would re-send"
    );
}

// --- resolution -----------------------------------------------------------

#[test]
fn a_file_source_drops_exactly_one_trailing_newline() {
    // Every `echo x > secret` writes one, and so does every orchestrator that mounts a
    // credential as a file. A store that kept it would send `Bearer sk_live_x\n` and
    // leave an operator staring at a 401 with a value that looks right.
    let plain = project_with(WEBHOOK, ALERT_EFFECT);
    let with_newline = project_with(&format!("{WEBHOOK}\n"), ALERT_EFFECT);

    let fingerprint = |dir: &TempDir| {
        let project = LoadedProject::load(dir.path());
        let (_, report) = hekla::secrets::resolve(&project.program, &project.config, &project.root);
        report[0].fingerprint.clone().expect("it resolved")
    };
    assert_eq!(
        fingerprint(&plain),
        fingerprint(&with_newline),
        "a trailing newline changed the credential"
    );

    // One, not all: a credential whose own last character is a newline is still
    // expressible, so the trim must not be a `trim_end`.
    let two = project_with(&format!("{WEBHOOK}\n\n"), ALERT_EFFECT);
    assert_ne!(fingerprint(&plain), fingerprint(&two));
}

#[test]
fn booting_without_a_required_credential_names_every_missing_one() {
    let dir = tempfile::tempdir().unwrap();
    let write = |rel: &str, body: &str| {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    };
    write("events/order.hk", EVENTS);
    write("commands/place-order.hk", PLACE_ORDER);
    write(
        "effects/alert.hk",
        r#"
secret ONE
secret TWO

effect Alert {
  on @order.placed as e { @key customer_id, order_id } {
    http.post(ONE, { "order": order_id })
    http.post(TWO, { "order": order_id })
  }
}
"#,
    );

    let data = tempfile::tempdir().unwrap();
    let err = Boot::new(dir.path())
        .data_dir(data.path())
        .try_start()
        .err()
        .expect("serving without the credentials must refuse");
    let message = format!("{err:#}");
    // Both at once. An operator fixing one credential per restart is the failure mode
    // `KeyStore::verify_masters_present` is shaped to avoid, and this follows it.
    assert!(message.contains("`ONE`"), "{message}");
    assert!(message.contains("`TWO`"), "{message}");
    assert!(
        message.contains("HEKLA_SECRET_ONE"),
        "it says where it looked: {message}"
    );
}

#[test]
fn an_optional_credential_may_be_unset() {
    let project = project_with(
        WEBHOOK,
        r#"
secret WEBHOOK_URL
secret FALLBACK_URL?

effect Alert {
  on @order.placed as e { @key customer_id, order_id } {
    let fallback = FALLBACK_URL
    if fallback.is_some() {
      http.post(fallback, { "order": order_id })
    }
    http.post(WEBHOOK_URL, { "order": order_id })
  }
}
"#,
    );
    let data = tempfile::tempdir().unwrap();
    // The whole reason the language has `secret NAME?`: an unset one is a branch the
    // program takes, not a boot failure.
    let booted = Boot::new(project.path())
        .data_dir(data.path())
        .http_status(200)
        .start();
    let report = booted.rt.secret_report();
    let fallback = report
        .iter()
        .find(|one| one.name == "FALLBACK_URL")
        .expect("it is reported even though it is unset");
    assert!(fallback.optional);
    assert!(!fallback.resolved());
    assert!(
        !fallback.missing(),
        "an unset optional does not stop a boot"
    );
    booted.shutdown();
}

// --- what `hekla check` will and will not say -----------------------------

#[test]
fn checking_never_needs_a_credential_to_be_set() {
    // `hekla check` is the CI gate. One that needed production credentials to pass
    // would either be run with them, which is worse than the problem it solves, or be
    // skipped. Whether a credential is set is `hekla plan`'s question and
    // `Runtime::open`'s refusal, never this one's.
    let dir = tempfile::tempdir().unwrap();
    let write = |rel: &str, body: &str| {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    };
    write("events/order.hk", EVENTS);
    write("commands/place-order.hk", PLACE_ORDER);
    write("effects/alert.hk", ALERT_EFFECT);

    let project = LoadedProject::load(dir.path());
    let findings = support::findings(&project);
    let errors: Vec<_> = findings
        .iter()
        .filter(|finding| finding.severity == Severity::Error)
        .collect();
    assert!(
        errors.is_empty(),
        "checking a project with unset credentials must pass: {errors:?}"
    );
}

#[test]
fn a_refused_boot_records_nothing_as_deployed() {
    // The refusal has to happen before `set_current_declarations`, or a boot that never
    // served still writes the candidate into the declaration table. The next `hekla
    // plan` would then compare the candidate against itself and report that nothing
    // would change, for a deploy that has not run.
    let project = project_with(WEBHOOK, ALERT_EFFECT);
    let data = tempfile::tempdir().unwrap();
    fs::remove_file(project.path().join("credential")).unwrap();

    let err = Boot::new(project.path())
        .data_dir(data.path())
        .try_start()
        .err()
        .expect("the boot must refuse");
    assert!(format!("{err:#}").contains("WEBHOOK_URL"));

    // The operational database is created before the refusal (opening one is harmless
    // and migrating an empty one changes nothing), so the check that matters is the
    // declaration table: nothing was recorded, so a plan still has the whole deploy
    // ahead of it rather than reporting that it has already happened.
    let out = Command::new(env!("CARGO_BIN_EXE_hekla"))
        .arg("plan")
        .arg(project.path())
        .arg("--data-dir")
        .arg(data.path())
        .arg("--json")
        .output()
        .unwrap();
    assert!(out.status.success());
    let plan: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let added: Vec<&serde_json::Value> = plan["changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|change| change["verdict"] == "added")
        .collect();
    assert!(
        added.iter().any(|change| change["name"] == "Alert"),
        "a refused boot recorded itself as deployed: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn an_unreadable_source_is_reported_rather_than_raised() {
    // A `/run/secrets` mount the plan's user cannot read is exactly when an operator
    // needs the declaration diff, so it must not be the one case that produces no plan.
    let project = project_with(WEBHOOK, ALERT_EFFECT);
    let credential = project.path().join("credential");
    fs::set_permissions(&credential, fs::Permissions::from_mode(0o000)).unwrap();

    let loaded = LoadedProject::load(project.path());
    let (_, report) = hekla::secrets::resolve(&loaded.program, &loaded.config, &loaded.root);
    let one = &report[0];
    assert!(!one.resolved());
    assert!(
        one.missing(),
        "a source that is there and unreadable stops a deploy"
    );
    let why = one.error.as_deref().expect("it says why");
    assert!(why.contains("WEBHOOK_URL"), "{why}");

    // And the refusal names the reason, so an operator is not sent looking for a file
    // that is right there.
    let refusal = hekla::secrets::refusal(&report, "serve").expect("it refuses");
    assert!(refusal.contains("Permission denied"), "{refusal}");

    fs::set_permissions(&credential, fs::Permissions::from_mode(0o644)).unwrap();
}

#[test]
fn an_unset_optional_credential_does_not_cost_replay_coverage() {
    // `MissingSecret` is the required-only backstop: an optional one answers an absent
    // `Opt(Secret)` and the handler branches, so an effect that reads one replays fine.
    // Counting it as uncovered would throw away real divergence coverage.
    let project = project_with(
        WEBHOOK,
        r#"
secret WEBHOOK_URL
secret FALLBACK_URL?

effect Alert {
  on @order.placed as e { @key customer_id, order_id } {
    let fallback = FALLBACK_URL
    if fallback.is_some() {
      http.post(fallback, { "order": order_id })
    }
    http.post(WEBHOOK_URL, { "order": order_id })
  }
}
"#,
    );
    let data = tempfile::tempdir().unwrap();
    let booted = Boot::new(project.path())
        .data_dir(data.path())
        .http_status(200)
        .start();
    place_order(&booted.rt);
    wait_until("the call to be journaled", || {
        !call_hashes(&open_op_db(data.path())).is_empty()
    });
    booted.shutdown();

    let out = Command::new(env!("CARGO_BIN_EXE_hekla"))
        .arg("plan")
        .arg(project.path())
        .arg("--data-dir")
        .arg(data.path())
        .arg("--json")
        .arg("--replay")
        .output()
        .unwrap();
    let plan: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        plan["coverage"]["no_secret"],
        0,
        "an unset optional made an effect look unreplayable: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn a_broken_project_is_not_also_told_its_credentials_are_undeclared() {
    // `LoadedProject::load` substitutes an empty program when the check fails, so every
    // configured credential would otherwise be reported as naming no declaration, on top
    // of the real error and advising the author to write what they already wrote.
    let dir = tempfile::tempdir().unwrap();
    let write = |rel: &str, body: &str| {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    };
    write("events/order.hk", EVENTS);
    write("commands/place-order.hk", PLACE_ORDER);
    write("effects/alert.hk", ALERT_EFFECT);
    // A real error: an undeclared event type.
    write(
        "commands/broken.hk",
        "command Broken(order_id: Uuid) {\n  emit @order.nope { order_id }\n}\n",
    );
    write("hekla.toml", "[secrets]\nWEBHOOK_URL = { env = \"W\" }\n");

    let project = LoadedProject::load(dir.path());
    let findings = support::findings(&project);
    assert!(
        !findings.iter().any(|finding| finding
            .message
            .contains("which the project does not declare")),
        "a failed check produced spurious credential advice: {findings:?}"
    );
}

#[test]
fn a_declared_credential_nothing_reads_is_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let write = |rel: &str, body: &str| {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    };
    write("events/order.hk", EVENTS);
    write("commands/place-order.hk", PLACE_ORDER);
    write(
        "effects/alert.hk",
        &format!("secret UNUSED\n{ALERT_EFFECT}"),
    );

    let project = LoadedProject::load(dir.path());
    let findings = support::findings(&project);
    assert!(
        findings
            .iter()
            .any(|finding| finding.severity == Severity::Warning
                && finding.message.contains("`UNUSED`")),
        "an unread credential asks a deployment for something nothing uses: {findings:?}"
    );
    // And the one that *is* read must not be warned about, or the lint is noise.
    assert!(
        !findings
            .iter()
            .any(|finding| finding.message.contains("`WEBHOOK_URL`")),
        "the credential the effect reads was reported as unused: {findings:?}"
    );
}

#[test]
fn a_config_entry_naming_no_declaration_is_a_warning() {
    let dir = project_with(WEBHOOK, ALERT_EFFECT);
    let toml = dir.path().join("hekla.toml");
    let existing = fs::read_to_string(&toml).unwrap();
    fs::write(&toml, format!("{existing}TYPOED = {{ env = \"NOPE\" }}\n")).unwrap();

    let project = LoadedProject::load(dir.path());
    let findings = support::findings(&project);
    assert!(
        findings
            .iter()
            .any(|finding| finding.severity == Severity::Warning
                && finding.message.contains("`TYPOED`")),
        "a [secrets] entry naming nothing is a line that does nothing: {findings:?}"
    );
}

// --- the CLI --------------------------------------------------------------

#[test]
fn the_secrets_command_is_a_gate_and_prints_no_credential() {
    let project = project_with(WEBHOOK, ALERT_EFFECT);
    let ok = Command::new(env!("CARGO_BIN_EXE_hekla"))
        .arg("secrets")
        .arg(project.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&ok.stdout);
    assert!(ok.status.success(), "a configured project passes: {stdout}");
    assert!(stdout.contains("WEBHOOK_URL"), "{stdout}");
    assert!(
        !stdout.contains("XXXXsupersecretXXXX"),
        "the report printed the credential: {stdout}"
    );

    // With the source gone it is a failure, so it stands on its own as a pre-deploy
    // gate for the thing `serve` would otherwise refuse to start over.
    fs::remove_file(project.path().join("credential")).unwrap();
    let missing = Command::new(env!("CARGO_BIN_EXE_hekla"))
        .arg("secrets")
        .arg(project.path())
        .output()
        .unwrap();
    assert!(!missing.status.success());
    let stdout = String::from_utf8_lossy(&missing.stdout);
    assert!(stdout.contains("MISSING"), "{stdout}");
}

#[test]
fn the_environment_fallback_needs_no_configuration() {
    // A project that configures nothing still resolves every credential it declares,
    // which is what keeps `[secrets]` optional. Driven as a subprocess so setting the
    // variable cannot race another test's read of the environment.
    let dir = tempfile::tempdir().unwrap();
    let write = |rel: &str, body: &str| {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    };
    write("events/order.hk", EVENTS);
    write("commands/place-order.hk", PLACE_ORDER);
    write("effects/alert.hk", ALERT_EFFECT);

    let out = Command::new(env!("CARGO_BIN_EXE_hekla"))
        .arg("secrets")
        .arg(dir.path())
        .env("HEKLA_SECRET_WEBHOOK_URL", WEBHOOK)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("env HEKLA_SECRET_WEBHOOK_URL"), "{stdout}");
    assert!(
        !stdout.contains("XXXXsupersecretXXXX"),
        "the report printed the credential: {stdout}"
    );
}

#[test]
fn a_plan_reports_an_unset_credential_and_is_not_empty() {
    let project = project_with(WEBHOOK, ALERT_EFFECT);
    let data = tempfile::tempdir().unwrap();
    Boot::new(project.path())
        .data_dir(data.path())
        .http_status(200)
        .start()
        .shutdown();

    // The same project, deployed, with the credential now unreachable. Nothing about
    // the code changed, so without the secrets section this plan would say "nothing
    // would change" about a deploy that will refuse to boot.
    fs::remove_file(project.path().join("credential")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_hekla"))
        .arg("plan")
        .arg(project.path())
        .arg("--data-dir")
        .arg(data.path())
        .arg("--json")
        .output()
        .unwrap();
    assert!(out.status.success(), "plan exits zero whatever it finds");
    let plan: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let secrets = plan["secrets"]
        .as_array()
        .expect("always present, never null");
    let one = &secrets[0];
    assert_eq!(one["name"], "WEBHOOK_URL");
    assert_eq!(one["resolved"], false);
    assert!(one["fingerprint"].is_null());

    let text = Command::new(env!("CARGO_BIN_EXE_hekla"))
        .arg("plan")
        .arg(project.path())
        .arg("--data-dir")
        .arg(data.path())
        .output()
        .unwrap();
    let rendered = String::from_utf8_lossy(&text.stdout);
    assert!(
        rendered.contains("would refuse to start"),
        "a plan must not call this deploy a no-op: {rendered}"
    );
    assert!(!rendered.contains("ok: nothing would change"), "{rendered}");
}
