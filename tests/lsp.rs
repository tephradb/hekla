//! The language server, driven over a real connection.
//!
//! The unit tests in `src/lsp/` call the context's methods directly. These drive
//! the actual protocol, because a great deal of the behaviour lives in what
//! `starlark_lsp` does with what kiln returns: which URIs the client is handed,
//! which requests reach the context at all, and what survives a document being
//! edited.

mod support;

use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use lsp_server::{Connection, Message, Notification, Request, RequestId};
use serde_json::{Value, json};
use support::{ORDER_EVENTS, ORDERS_PROJECTOR, PLACE_ORDER, write_project};
use tempfile::TempDir;

/// How long to wait for any single response. Generous: this bounds a hang, it
/// does not measure speed.
const BUDGET: Duration = Duration::from_secs(30);

/// A running server and the client end of its connection.
struct Lsp {
    client: Connection,
    server: Option<JoinHandle<()>>,
    root: PathBuf,
    next_id: i32,
    /// Notifications that arrived while waiting for a response. Diagnostics are
    /// published asynchronously, so they interleave with everything.
    pending: Vec<Notification>,
}

impl Lsp {
    fn start(root: &Path) -> Lsp {
        let (client, server) = Connection::memory();
        let handle = thread::spawn(move || {
            kiln::lsp::serve(server, true).expect("the server should shut down cleanly");
        });

        let mut lsp = Lsp {
            client,
            server: Some(handle),
            root: root.to_path_buf(),
            next_id: 0,
            pending: Vec::new(),
        };
        let root_uri = uri_of(root);
        lsp.request(
            "initialize",
            json!({
                "capabilities": {},
                "workspaceFolders": [{"uri": root_uri, "name": "project"}],
            }),
        );
        lsp.notify("initialized", json!({}));
        lsp
    }

    fn uri(&self, rel: &str) -> String {
        uri_of(&self.root.join(rel))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = RequestId::from(self.next_id);
        self.client
            .sender
            .send(Message::Request(Request {
                id: id.clone(),
                method: method.to_owned(),
                params,
            }))
            .unwrap();

        let deadline = Instant::now() + BUDGET;
        loop {
            match self.client.receiver.recv_timeout(deadline - Instant::now()) {
                Ok(Message::Response(response)) if response.id == id => {
                    if let Some(err) = response.error {
                        panic!("{method} failed: {err:?}");
                    }
                    return response.result.unwrap_or(Value::Null);
                }
                Ok(Message::Notification(notification)) => self.pending.push(notification),
                Ok(other) => panic!("unexpected message while awaiting {method}: {other:?}"),
                Err(err) => panic!("{method} timed out: {err}"),
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.client
            .sender
            .send(Message::Notification(Notification {
                method: method.to_owned(),
                params,
            }))
            .unwrap();
    }

    /// Open a document and return the diagnostics published for it.
    fn open(&mut self, rel: &str, text: &str) -> Vec<Value> {
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": self.uri(rel),
                "languageId": "starlark",
                "version": 1,
                "text": text,
            }}),
        );
        self.diagnostics(rel)
    }

    /// Replace a document's contents and return the diagnostics that follow.
    fn change(&mut self, rel: &str, version: i32, text: &str) -> Vec<Value> {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": self.uri(rel), "version": version},
                "contentChanges": [{"text": text}],
            }),
        );
        self.diagnostics(rel)
    }

    /// Wait for the next diagnostics published for `rel`.
    fn diagnostics(&mut self, rel: &str) -> Vec<Value> {
        let uri = self.uri(rel);
        let deadline = Instant::now() + BUDGET;
        loop {
            if let Some(index) = self.pending.iter().position(|notification| {
                notification.method == "textDocument/publishDiagnostics"
                    && notification.params["uri"] == Value::String(uri.clone())
            }) {
                let notification = self.pending.remove(index);
                return notification.params["diagnostics"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
            }
            match self.client.receiver.recv_timeout(deadline - Instant::now()) {
                Ok(Message::Notification(notification)) => self.pending.push(notification),
                Ok(other) => panic!("unexpected message while awaiting diagnostics: {other:?}"),
                Err(err) => panic!("no diagnostics for {rel}: {err}"),
            }
        }
    }

    fn at(&self, rel: &str, line: u32, character: u32) -> Value {
        json!({
            "textDocument": {"uri": self.uri(rel)},
            "position": {"line": line, "character": character},
        })
    }

    fn shutdown(mut self) {
        self.request("shutdown", Value::Null);
        self.notify("exit", Value::Null);
        if let Some(handle) = self.server.take() {
            handle.join().expect("the server thread should not panic");
        }
    }
}

fn uri_of(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// Only the messages, for readable assertions.
fn messages(diagnostics: &[Value]) -> Vec<String> {
    diagnostics
        .iter()
        .map(|d| d["message"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// Severity 1 is Error in the protocol.
fn errors(diagnostics: &[Value]) -> Vec<String> {
    diagnostics
        .iter()
        .filter(|d| d["severity"] == json!(1))
        .map(|d| d["message"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn orders_project() -> TempDir {
    write_project(&[
        ("events/order.star", ORDER_EVENTS),
        ("commands/place-order.star", PLACE_ORDER),
        ("projectors/orders.star", ORDERS_PROJECTOR),
    ])
}

/// The regression the server exists for. Every one of these builtins is undefined
/// to a generic Starlark server, and `load("events/order.star", ...)` resolves
/// nowhere without kiln's rules.
#[test]
fn a_correct_project_produces_no_diagnostics() {
    let dir = orders_project();
    let mut lsp = Lsp::start(dir.path());

    for (rel, source) in [
        ("events/order.star", ORDER_EVENTS),
        ("commands/place-order.star", PLACE_ORDER),
        ("projectors/orders.star", ORDERS_PROJECTOR),
    ] {
        let diagnostics = lsp.open(rel, source);
        assert!(
            diagnostics.is_empty(),
            "{rel} should be clean, got {:?}",
            messages(&diagnostics)
        );
    }
    lsp.shutdown();
}

/// The builtins follow the directory, which is the thing a stub file cannot say.
#[test]
fn a_builtin_from_another_role_is_an_error() {
    let dir = orders_project();
    let mut lsp = Lsp::start(dir.path());

    let source = r#"
load("events/order.star", "order_placed")

orders = entity(key = "order_id", fields = {"order_id": uuid()})

handle = {order_placed(): lambda event: [put(orders, {"order_id": now()})]}
"#;
    let diagnostics = lsp.open("projectors/clock.star", source);
    let errors = errors(&diagnostics);
    assert!(
        errors.iter().any(|message| message.contains("now")),
        "expected `now` to be undefined in a projector, got {errors:?}"
    );
    lsp.shutdown();
}

#[test]
fn an_illegal_load_is_reported_over_the_load_statement() {
    let dir = orders_project();
    let mut lsp = Lsp::start(dir.path());

    let diagnostics = lsp.open(
        "projectors/bad.star",
        "load(\"commands/place-order.star\", \"handle\")\n",
    );
    let load = diagnostics
        .iter()
        .find(|d| {
            d["message"]
                .as_str()
                .unwrap_or_default()
                .contains("may only load from events/ or lib/")
        })
        .unwrap_or_else(|| panic!("{:?}", messages(&diagnostics)));
    assert_eq!(load["range"]["start"]["line"], json!(0));
    lsp.shutdown();
}

#[test]
fn a_load_of_a_file_that_does_not_exist_is_reported() {
    let dir = orders_project();
    let mut lsp = Lsp::start(dir.path());

    let diagnostics = lsp.open(
        "projectors/bad.star",
        "load(\"events/odrer.star\", \"order_placed\")\n",
    );
    assert!(
        errors(&diagnostics)
            .iter()
            .any(|message| message.contains("no such file in this project")),
        "{:?}",
        messages(&diagnostics)
    );
    lsp.shutdown();
}

/// Goto-definition on a builtin, all the way through: a `starlark:` URI the
/// client can then fetch, whose contents carry the symbol.
#[test]
fn goto_definition_on_a_builtin_reaches_a_generated_stub() {
    let dir = write_project(&[
        ("events/order.star", ORDER_EVENTS),
        (
            "effects/notify.star",
            "load(\"events/order.star\", \"order_placed\")\n\ndef on_placed(event):\n    http.post(url = \"https://example.test\")\n\nhandle = {order_placed(): on_placed}\n",
        ),
    ]);
    let mut lsp = Lsp::start(dir.path());
    let source = std::fs::read_to_string(dir.path().join("effects/notify.star")).unwrap();
    lsp.open("effects/notify.star", &source);

    // The `http` of `http.post` on line 3.
    let links = lsp.request(
        "textDocument/definition",
        lsp.at("effects/notify.star", 3, 4),
    );
    let target = links[0]["targetUri"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a definition, got {links}"));
    assert!(target.starts_with("starlark:"), "{target}");

    // And the client can fetch it, which is what makes the jump usable.
    let contents = lsp.request("starlark/fileContents", json!({"uri": target}));
    let source = contents["contents"].as_str().expect("stub contents");
    assert!(source.contains("http = struct("), "{source}");
    lsp.shutdown();
}

#[test]
fn hover_on_a_kiln_builtin_shows_its_documentation() {
    let dir = orders_project();
    let mut lsp = Lsp::start(dir.path());
    let source = "input = schema(order_id = uuid())\n\ndef handle(input, state):\n    return reject(\"no\", \"not allowed\")\n";
    lsp.open("commands/deny.star", source);

    // The `reject` on line 3.
    let hover = lsp.request("textDocument/hover", lsp.at("commands/deny.star", 3, 12));
    let rendered = hover["contents"].to_string();
    assert!(rendered.contains("reject"), "{rendered}");
    assert!(
        rendered.contains("state") || rendered.contains("code"),
        "expected kiln's own docs for reject, got {rendered}"
    );
    lsp.shutdown();
}

/// Completion inside a `load()` path offers exactly what kiln permits, which
/// turns the restriction into a list rather than a rule to trip over.
#[test]
fn load_path_completion_offers_only_loadable_modules() {
    let dir = write_project(&[
        ("events/order.star", ORDER_EVENTS),
        ("lib/validation.star", "def ok():\n    return True\n"),
        ("commands/place-order.star", PLACE_ORDER),
    ]);
    let mut lsp = Lsp::start(dir.path());
    lsp.open("commands/draft.star", "load(\"\", \"x\")\n");

    // Inside the empty string on line 0.
    let response = lsp.request(
        "textDocument/completion",
        lsp.at("commands/draft.star", 0, 6),
    );
    let items = response.as_array().cloned().unwrap_or_default();
    let labels: Vec<&str> = items
        .iter()
        .filter_map(|item| item["label"].as_str())
        .collect();

    assert!(labels.contains(&"events/order.star"), "{labels:?}");
    assert!(labels.contains(&"lib/validation.star"), "{labels:?}");
    assert!(
        !labels.contains(&"commands/place-order.star"),
        "a command is not loadable, so it must not be offered: {labels:?}"
    );
    lsp.shutdown();
}

#[test]
fn a_syntax_error_appears_and_then_clears() {
    let dir = orders_project();
    let mut lsp = Lsp::start(dir.path());

    let diagnostics = lsp.open("commands/place-order.star", PLACE_ORDER);
    assert!(diagnostics.is_empty(), "{:?}", messages(&diagnostics));

    let diagnostics = lsp.change("commands/place-order.star", 2, "input = schema(\n");
    assert_eq!(diagnostics.len(), 1, "{:?}", messages(&diagnostics));

    let diagnostics = lsp.change("commands/place-order.star", 3, PLACE_ORDER);
    assert!(
        diagnostics.is_empty(),
        "the error should clear, got {:?}",
        messages(&diagnostics)
    );
    lsp.shutdown();
}

/// The server handles one message at a time on one thread, so a slow check would
/// show up as an unresponsive editor rather than as slow diagnostics. This is the
/// test that would catch that.
#[test]
fn the_server_stays_responsive_under_rapid_edits() {
    let dir = orders_project();
    let mut lsp = Lsp::start(dir.path());
    lsp.open("commands/place-order.star", PLACE_ORDER);

    for version in 2..52 {
        let source = format!("{PLACE_ORDER}\n# edit {version}\n");
        lsp.change("commands/place-order.star", version, &source);
    }

    let started = Instant::now();
    let hover = lsp.request(
        "textDocument/hover",
        lsp.at("commands/place-order.star", 3, 12),
    );
    assert!(
        started.elapsed() < BUDGET,
        "hover took {:?} after 50 edits",
        started.elapsed()
    );
    assert!(hover.is_object() || hover.is_null());
    lsp.shutdown();
}

/// Stdout is the protocol. A module body that prints must not corrupt it, which
/// it would if evaluation ever wrote to stdout.
#[test]
fn a_module_that_prints_does_not_corrupt_the_stream() {
    let dir = orders_project();
    let mut lsp = Lsp::start(dir.path());

    lsp.open(
        "commands/noisy.star",
        "print(\"hello from a module body\")\n\ninput = schema()\n\ndef handle(input, state):\n    return None\n",
    );
    // If the print had reached stdout, this exchange would not parse.
    let hover = lsp.request("textDocument/hover", lsp.at("commands/noisy.star", 2, 0));
    assert!(hover.is_object() || hover.is_null());
    lsp.shutdown();
}

/// Every `.star` file under a project's module directories.
fn module_files(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for subdir in [
        "events",
        "lib",
        "commands",
        "projectors",
        "effects",
        "tests",
    ] {
        let dir = root.join(subdir);
        if !dir.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&dir).sort_by_file_name() {
            let entry = entry.unwrap();
            if entry.file_type().is_file()
                && entry.path().extension().and_then(|e| e.to_str()) == Some("star")
            {
                let rel = entry.path().strip_prefix(root).unwrap();
                found.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    found
}

/// What `kiln check` says about a project, as `(location, message)` pairs.
fn check_findings(root: &Path) -> Vec<(String, String)> {
    let project = kiln::loader::LoadedProject::load(root);
    let mut findings = project.findings.clone();
    findings.extend(kiln::validate::check(&project));
    findings
        .into_iter()
        .filter(|finding| finding.severity == kiln::loader::Severity::Error)
        .map(|finding| (finding.location, finding.message))
        .collect()
}

/// The rule the design serves: the editor never reports a problem `kiln check`
/// would not. An editor that invents errors teaches people to ignore it.
///
/// The shipped examples are correct, so for them the subset is the empty set,
/// which is the strongest form of the claim and the one people actually meet.
#[test]
fn the_examples_produce_no_diagnostics() {
    for name in ["users", "orders"] {
        let root = support::example_dir(name);
        assert!(
            check_findings(&root).is_empty(),
            "{name} should be clean to `kiln check` first"
        );

        let mut lsp = Lsp::start(&root);
        for rel in module_files(&root) {
            let source = std::fs::read_to_string(root.join(&rel)).unwrap();
            let diagnostics = lsp.open(&rel, &source);
            assert!(
                errors(&diagnostics).is_empty(),
                "{name}/{rel} should be clean, got {:?}",
                messages(&diagnostics)
            );
        }
        lsp.shutdown();
    }
}

/// And with real errors present, everything the editor reports is something
/// `kiln check` reports too.
///
/// Each fixture file carries exactly one defect on purpose. The editor can report
/// *more* than `kiln check` for a single file and still be right: name resolution
/// finds every undefined name, where evaluation stops at the first. What must
/// never happen is a problem of a kind `kiln check` does not have at all.
#[test]
fn every_reported_error_is_one_kiln_check_also_reports() {
    let dir = write_project(&[
        ("events/order.star", ORDER_EVENTS),
        // Three different failures: an illegal load, a builtin from another role,
        // and a missing required binding.
        (
            "commands/bad-load.star",
            "load(\"projectors/orders.star\", \"orders\")\n\ninput = schema()\n\ndef handle(input, state):\n    return None\n",
        ),
        (
            "commands/bad-name.star",
            // `reveal` is an effect builtin, so a command never sees it.
            "input = schema()\n\ndef handle(input, state):\n    return reveal(input)\n",
        ),
        ("commands/no-handle.star", "input = schema()\n"),
        ("projectors/orders.star", ORDERS_PROJECTOR),
    ]);
    let expected = check_findings(dir.path());
    assert!(!expected.is_empty(), "the fixture should be broken");

    let mut lsp = Lsp::start(dir.path());
    let mut seen = 0usize;
    for rel in module_files(dir.path()) {
        let source = std::fs::read_to_string(dir.path().join(&rel)).unwrap();
        let diagnostics = lsp.open(&rel, &source);
        for message in errors(&diagnostics) {
            seen += 1;
            assert!(
                expected.iter().any(|(location, expected_message)| {
                    *location == rel
                        && (expected_message.contains(&message)
                            || message.contains(expected_message.as_str()))
                }),
                "the editor reported `{message}` for {rel}, which `kiln check` does not: {expected:?}"
            );
        }
    }
    assert!(seen >= 3, "expected the fixture's errors to be reported");
    lsp.shutdown();
}
