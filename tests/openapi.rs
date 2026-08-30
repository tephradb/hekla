//! The generated OpenAPI document, over a real project.
//!
//! The unit tests in `src/openapi.rs` drive a hand-built [`Surface`]; these drive the
//! two paths a user actually reaches, `GET /openapi.json` and `hekla openapi`, over
//! `examples/users` (three commands, two projectors, four event types, and a
//! subject-encrypted field). What they add on top of the unit tests is coverage of the
//! wiring: that the served document is generated from the loaded project, that the CLI
//! and the server produce the same one, and that no routed path is left undescribed.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;

mod support;

use hekla::server::routes;
use support::{boot_example, example_dir, load_ok};

/// The document a booted runtime holds, parsed once for the whole binary.
///
/// Booting `examples/users` opens tephra segments and the op-DB and starts a thread
/// per projector and effect. The assertions below are read-only over one static
/// string, so they share a single boot rather than paying for one each.
///
/// This reads the runtime's own field. Exactly one test below fetches it over HTTP and
/// asserts the two match, which is what keeps this shortcut honest.
fn served_document() -> &'static Value {
    static DOC: OnceLock<Value> = OnceLock::new();
    DOC.get_or_init(|| {
        let harness = boot_example();
        let doc =
            serde_json::from_str(harness.rt.openapi_json()).expect("the runtime serves valid JSON");
        harness.shutdown();
        doc
    })
}

fn generated_document() -> Value {
    let project = load_ok(&example_dir("users"));
    hekla::openapi::build(&hekla::openapi::Surface::from_project(&project))
}

/// Whether `documented` describes `route`, allowing for the concrete expansion the
/// generator does: `/commands/{name}` becomes `/commands/RegisterUser`, and
/// `/read/{projector}/{entity}` becomes `/read/Users/User`.
fn describes(documented: &str, route: &str) -> bool {
    let route_segments: Vec<&str> = route.split('/').collect();
    let documented_segments: Vec<&str> = documented.split('/').collect();
    if route_segments.len() != documented_segments.len() {
        return false;
    }
    route_segments
        .iter()
        .zip(&documented_segments)
        .all(|(expected, actual)| {
            // A template segment matches a concrete expansion, but only when the
            // document did not keep the template itself: `{key}` stays `{key}`.
            expected == actual || (expected.starts_with('{') && !actual.starts_with('{'))
        })
}

/// A route with no path in the document is surface with no spec, which is the gap this
/// whole generator exists to close.
///
/// `ROUTES` is `server`'s own list, the same constants `app()` registers, so the two
/// cannot drift by a typo. Adding a route still means adding it there.
#[test]
fn every_routed_path_is_described() {
    let doc = served_document();
    let paths: Vec<&str> = doc["paths"]
        .as_object()
        .expect("the document has paths")
        .keys()
        .map(String::as_str)
        .collect();
    for route in routes() {
        assert!(
            paths.iter().any(|documented| describes(documented, route)),
            "route `{route}` has no path in the document; documented: {paths:?}"
        );
    }
}

/// The generator reads the project, so the document has to name the project's own
/// modules rather than a generic template.
#[test]
fn the_document_describes_the_projects_real_modules() {
    let doc = served_document();
    let paths = doc["paths"].as_object().unwrap();

    // Commands: public routed, internal absent.
    assert!(paths.contains_key("/commands/RegisterUser"));
    assert!(paths.contains_key("/commands/RenameUser"));
    assert!(
        !paths.keys().any(|path| path.contains("RecordWelcome")),
        "an internal command is not routed, so it must not be documented: {:?}",
        paths.keys().collect::<Vec<_>>()
    );

    // Read models: both projectors, both operations each.
    for path in [
        "/read/Users/User",
        "/read/Users/User/{key}",
        "/read/UserStats/Totals",
        "/read/UserStats/Totals/{key}",
    ] {
        assert!(paths.contains_key(path), "missing read path {path}");
    }

    // One tag per projector, in the declared render order.
    let tags: Vec<&str> = doc["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tag| tag["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        tags,
        vec![
            "commands",
            "read: UserStats",
            "read: Users",
            "operations",
            "introspection",
        ],
    );

    // Events: every declared type gets a schema, and the emitted-event enum agrees.
    let schemas = doc["components"]["schemas"].as_object().unwrap();
    for event_type in [
        "user.registered",
        "user.renamed",
        "user.welcomed",
        "reminder.scheduled",
    ] {
        assert!(
            schemas.contains_key(&format!("event.{event_type}")),
            "no schema for event `{event_type}`"
        );
    }
    let declared: Vec<&str> = schemas["EmittedEvent"]["properties"]["type"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert_eq!(
        declared,
        vec![
            "reminder.scheduled",
            "user.registered",
            "user.renamed",
            "user.welcomed",
        ],
    );
}

/// The three ways to obtain the document must all yield the same one: over HTTP, from
/// the `hekla openapi` binary, and from the library call both go through.
///
/// The HTTP leg is what makes `served_document`'s shortcut honest, since that reads the
/// runtime's field directly rather than going through the endpoint. The CLI leg is what
/// makes the subcommand trustworthy for committing a spec and diffing it in CI, and it
/// pins the detail that makes it usable at all: findings go to stderr, so
/// `hekla openapi . > openapi.json` writes a file `jq` will parse. Every other
/// subcommand prints them to stdout.
#[tokio::test]
async fn the_document_is_the_same_over_http_from_the_cli_and_from_the_library() {
    let harness = boot_example();
    let request = Request::builder()
        .method(Method::GET)
        .uri("/openapi.json")
        .body(Body::empty())
        .unwrap();
    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json"),
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let over_http: Value = serde_json::from_slice(&bytes).unwrap();
    harness.shutdown();

    let output = Command::new(env!("CARGO_BIN_EXE_hekla"))
        .arg("openapi")
        .arg(example_dir("users"))
        .output()
        .expect("running `hekla openapi`");
    assert!(
        output.status.success(),
        "`hekla openapi` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let dumped: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "stdout is not pure JSON ({err}); it began: {:?}",
            String::from_utf8_lossy(&output.stdout)
                .chars()
                .take(200)
                .collect::<String>()
        )
    });

    assert_eq!(
        &over_http,
        served_document(),
        "the endpoint and the runtime's own field disagree"
    );
    assert_eq!(dumped, over_http, "the CLI and the endpoint disagree");
    assert_eq!(
        dumped,
        generated_document(),
        "the CLI and the library agree, so a future divergence lands on one of them"
    );
}

/// A path that is not a hekla project must fail, not produce a stub.
///
/// `LoadedProject::load` reports no findings for a root it cannot find: it discovers
/// nothing and succeeds. Every other subcommand tolerates that, but this one's output
/// gets committed, so a regeneration step run from the wrong directory would replace a
/// real spec with the handful of operator paths that exist regardless, and exit 0.
#[test]
fn the_cli_refuses_a_path_that_is_not_a_project() {
    let missing = Command::new(env!("CARGO_BIN_EXE_hekla"))
        .arg("openapi")
        .arg("/nonexistent-dir-xyz")
        .output()
        .expect("running `hekla openapi`");
    assert!(
        !missing.status.success(),
        "a missing directory exited 0 with: {}",
        String::from_utf8_lossy(&missing.stdout)
    );
    assert!(missing.stdout.is_empty(), "a stub document was printed");
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("is not a directory"),
        "stderr should name the problem: {}",
        String::from_utf8_lossy(&missing.stderr)
    );

    // A real directory that simply is not a project fails the same way, which is the
    // likelier mistake: running from a repo root instead of the project subdirectory.
    let empty = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_hekla"))
        .arg("openapi")
        .arg(empty.path())
        .output()
        .expect("running `hekla openapi`");
    assert!(!output.status.success(), "an empty directory exited 0");
    assert!(output.stdout.is_empty(), "a stub document was printed");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("declares no commands"),
        "stderr should say the directory holds no project: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A project with errors gets no document at all, and says why on stderr.
#[test]
fn the_cli_refuses_to_generate_for_a_broken_project() {
    let project = support::write_project(&[
        ("events/e.hk", "event @thing.happened { id: Uuid }\n"),
        (
            "commands/broken.hk",
            "command Broken(id: Uuid) {\n  emit @no.such.event { id }\n}\n",
        ),
    ]);
    let output = Command::new(env!("CARGO_BIN_EXE_hekla"))
        .arg("openapi")
        .arg(project.path())
        .output()
        .expect("running `hekla openapi`");
    assert!(
        !output.status.success(),
        "a broken project must exit non-zero"
    );
    assert!(
        output.stdout.is_empty(),
        "a partial document is worse than none: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("refusing to generate"),
        "stderr should say why: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `hekla openapi` reads a project directory and nothing else. If it ever started
/// touching the data directory it would take the lock and stop being usable in CI
/// against a live deployment's checkout.
#[test]
fn generating_the_document_touches_no_data_directory() {
    // Sorted: `read_dir` yields in whatever order the filesystem chooses, and POSIX
    // promises none, so comparing raw listings could fail on identical contents.
    let listing = |dir: &Path| {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect();
        names.sort();
        names
    };
    let dir = example_dir("users");
    let before = listing(&dir);
    let doc = generated_document();
    assert!(doc["paths"].as_object().is_some_and(|p| !p.is_empty()));
    assert_eq!(
        before,
        listing(&dir),
        "generating the document created something"
    );
}

/// The `Status` schema against the body `GET /status` actually returns.
///
/// The document declares `additionalProperties: false` and a `required` list, so it
/// makes two promises about that body and nothing was checking either. The port broke
/// both at once: it deleted the fold-chunking machinery and `/status`'s `folds` counter
/// with it, and left the schema declaring `folds` required, so the published contract
/// promised a key the server had stopped sending and the admin console crashed reading
/// it.
#[tokio::test]
async fn the_status_body_matches_the_schema_that_describes_it() {
    let harness = boot_example();
    let response = harness
        .app()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    harness.shutdown();

    let schema = &served_document()["components"]["schemas"]["Status"];
    let declared: Vec<&str> = schema["properties"]
        .as_object()
        .expect("Status declares properties")
        .keys()
        .map(String::as_str)
        .collect();
    let returned: Vec<&str> = body
        .as_object()
        .expect("the status body is an object")
        .keys()
        .map(String::as_str)
        .collect();

    for name in schema["required"].as_array().unwrap() {
        let name = name.as_str().unwrap();
        assert!(
            returned.contains(&name),
            "the schema requires `{name}` and the body does not carry it: {returned:?}"
        );
    }
    // `additionalProperties: false` is the other half of the promise, so a key the
    // server grew without describing it fails here too.
    for name in &returned {
        assert!(
            declared.contains(name),
            "the body carries `{name}` and the schema does not describe it: {declared:?}"
        );
    }
}
