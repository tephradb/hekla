//! What the editor underlines.
//!
//! Three tiers, cheapest first, each producing [`EvalMessage`]s so there is one
//! place that turns a problem into an LSP diagnostic:
//!
//! - **parse**, always, and the only tier when a file will not parse;
//! - **resolve**, pure AST work: kiln's `load()` rules and name resolution
//!   against the builtins the file's directory actually gets;
//! - **project**, evaluating the buffer against the project's shared modules and
//!   running kiln's own shape and clause checks over the result.
//!
//! The tiers exist so the expensive one can be switched off and the useful ones
//! remain, and so a slow project cannot make typing slow: everything shared is
//! cached elsewhere, and what happens per keystroke is one parse and one module
//! evaluation.
//!
//! The rule the whole design serves: **never report a problem `kiln check` would
//! not report.** An editor that invents errors is worse than no editor support,
//! because it teaches people to ignore it.

use std::collections::HashMap;
use std::path::Path;

use starlark::analysis::{AstModuleLint, EvalMessage, EvalSeverity};
use starlark::codemap::ResolvedSpan;
use starlark::environment::Globals;
use starlark::syntax::AstModule;
use starlark::syntax::ast::{AssignTargetP, AstStmt, StmtP};
use starlark::typing::AstModuleTypecheck;

use crate::loader::{Finding, Severity, is_library_path, normalize_load_path};
use crate::lsp::env::Env;
use crate::lsp::project::{Located, Snapshot};
use crate::starlark_builtins::{module_def_from_frozen, module_name_from_path, parse_module};
use crate::{testing, validate};

/// The diagnostic name for a broken `load()`. A file with one is not evaluated,
/// exactly as the loader does not evaluate it.
const LOAD_RULE: &str = "kiln-load";

/// The lints worth showing, and how loudly.
///
/// An allowlist rather than a denylist. Most of starlark-rust's lints are either
/// already disabled upstream or aimed at large generated Bazel files: unused
/// bindings are the *product* of a kiln module, and a bare expression statement
/// is ordinary. The severities here are kiln's own judgement, not upstream's,
/// which is why they are restated rather than inherited.
const LINTS: [(&str, EvalSeverity); 7] = [
    // Two bindings of the same name: the second silently wins, so one of the
    // author's two `handle`s or `fold`s is dead.
    ("duplicate-top-level-assign", EvalSeverity::Error),
    // A repeated dict key drops an arm of a dispatch map or a field of a schema.
    ("duplicate-key", EvalSeverity::Error),
    ("using-unassigned", EvalSeverity::Error),
    ("using-maybe-undefined", EvalSeverity::Warning),
    ("missing-return-expression", EvalSeverity::Warning),
    ("unreachable", EvalSeverity::Warning),
    ("misplaced-load", EvalSeverity::Warning),
];

/// Diagnose a document that is not inside a kiln project: it parses or it does
/// not, and nothing more can honestly be said about it.
pub(crate) fn unlocated(rel: &str, src: String) -> (Option<AstModule>, Vec<EvalMessage>) {
    match parse_module(rel, src) {
        Err(err) => (None, vec![EvalMessage::from_error(Path::new(rel), &err)]),
        Ok(ast) => {
            // Guessing a globals set would be the starpls failure this server
            // exists to fix, so say what is actually wrong instead.
            let hint = EvalMessage {
                path: rel.to_owned(),
                span: None,
                severity: EvalSeverity::Advice,
                name: "kiln-not-a-module".to_owned(),
                description:
                    "not a kiln module: kiln loads .star files under events/, lib/, commands/, \
                     projectors/, effects/ or tests/ in a project directory, so this file is \
                     never loaded and its builtins are unknown"
                        .to_owned(),
                full_error_with_span: None,
                original: None,
            };
            (Some(ast), vec![hint])
        }
    }
}

/// Diagnose a document inside a kiln project.
///
/// Returns the parsed module so the server can keep serving hover, completion and
/// goto-definition from it.
pub(crate) fn diagnose(
    located: &Located,
    src: String,
    globals: &Globals,
    env: Env,
    snapshot: Option<&Snapshot>,
) -> (Option<AstModule>, Vec<EvalMessage>) {
    let rel = located.rel.as_str();
    let ast = match parse_module(rel, src) {
        Ok(ast) => ast,
        // A file that will not parse gets exactly one diagnostic, and no AST: the
        // server then keeps the last one that did parse, so navigation still works
        // mid-edit.
        Err(err) => return (None, vec![EvalMessage::from_error(Path::new(rel), &err)]),
    };

    let mut messages = resolve_tier(&ast, located, globals);
    // A file whose loads are illegal is never evaluated by the loader either, so
    // stopping here is what keeps the editor agreeing with `kiln check`: evaluating
    // it anyway would report the same broken import a second time, in the words the
    // load-time resolver uses rather than the ones the loader reports.
    let loads_resolve = !messages.iter().any(|message| message.name == LOAD_RULE);
    if let (Some(snapshot), true) = (snapshot, loads_resolve) {
        messages.extend(project_tier(&ast, located, globals, env, snapshot));
    }
    dedupe(&mut messages);
    (Some(ast), messages)
}

/// Parse-only checks: kiln's `load()` rules, name resolution, and the lints.
fn resolve_tier(ast: &AstModule, located: &Located, globals: &Globals) -> Vec<EvalMessage> {
    let mut messages = check_loads(ast, located);
    messages.extend(check_names(ast, globals, &located.rel));
    messages.extend(check_lints(ast, &located.rel));
    messages
}

/// Kiln's `load()` restriction, reported where the load is written.
///
/// The loader reaches these too, but only for files it walks, and only once the
/// whole project has been read. Doing it here costs a walk of the load statements
/// and gives the answer as the import is typed.
fn check_loads(ast: &AstModule, located: &Located) -> Vec<EvalMessage> {
    let mut messages = Vec::new();
    for load in ast.loads() {
        let span = load.span.resolve_span();
        let describe = |description: String| EvalMessage {
            path: located.rel.clone(),
            span: Some(span),
            severity: EvalSeverity::Error,
            name: LOAD_RULE.to_owned(),
            description,
            full_error_with_span: None,
            original: None,
        };
        match normalize_load_path(load.module_id) {
            Err(message) => messages.push(describe(message)),
            Ok(normalized) if !is_library_path(&normalized) => {
                messages.push(describe(format!(
                    "load(\"{}\") is not allowed; a {} may only load from events/ or lib/",
                    load.module_id,
                    located.role.label()
                )));
            }
            Ok(normalized) => {
                // Resolution happens at evaluation for the loader, which means a
                // typo in a path is only caught once everything else is right.
                if !located.root.join(&normalized).is_file() {
                    messages.push(describe(format!(
                        "load(\"{}\") could not be resolved: no such file in this project",
                        load.module_id
                    )));
                }
            }
        }
    }
    messages
}

/// Undefined names, resolved against the builtins this file's directory gets.
///
/// This is the diagnosis the whole server exists for. It runs the compiler's own
/// scope resolution rather than the `using-undefined` lint, because that reports
/// a misspelling as "use of undefined variable" where this one offers the name
/// that was probably meant.
///
/// Only scope errors are kept. The same pass produces type errors, which are not
/// reported: they have a history of false positives, they do not check lambdas,
/// and they degrade to `Any` over kiln's own value types, so they would fail the
/// rule that the editor never invents an error.
fn check_names(ast: &AstModule, globals: &Globals, rel: &str) -> Vec<EvalMessage> {
    let (errors, _, _, _) = ast.clone().typecheck(globals, &HashMap::new());
    errors
        .iter()
        .filter(|err| matches!(err.kind(), starlark::ErrorKind::Scope(_)))
        .map(|err| EvalMessage::from_error(Path::new(rel), err))
        .collect()
}

fn check_lints(ast: &AstModule, rel: &str) -> Vec<EvalMessage> {
    // No globals: undefined names are `check_names`'s job, and passing them here
    // would report each one twice in different words.
    ast.lint(None)
        .into_iter()
        .filter_map(|lint| {
            let (_, severity) = LINTS.iter().find(|(name, _)| *name == lint.short_name)?;
            let mut message = EvalMessage::from(lint);
            message.severity = *severity;
            message.path = rel.to_owned();
            Some(message)
        })
        .collect()
}

/// Evaluate the buffer against the project's shared modules, then run kiln's own
/// checks over the result: the ones that need a value, not just a syntax tree.
fn project_tier(
    ast: &AstModule,
    located: &Located,
    globals: &Globals,
    env: Env,
    snapshot: &Snapshot,
) -> Vec<EvalMessage> {
    let rel = located.rel.as_str();
    let frozen =
        match snapshot
            .project
            .eval_ast_against_libraries(ast.clone(), globals, env.query_mode())
        {
            Ok(frozen) => frozen,
            // One error, and stop: evaluation does not continue past a failure, so
            // anything after this would be speculation.
            Err(err) => return vec![EvalMessage::from_error(Path::new(rel), &err)],
        };

    let mut findings = Vec::new();
    match located.role.module_kind() {
        Some(kind) => {
            let name = match module_name_from_path(rel) {
                Ok(name) => name,
                Err(err) => {
                    findings.push(Finding::error(rel, format!("{err:#}")));
                    return findings
                        .iter()
                        .map(|finding| finding_message(finding, ast))
                        .collect();
                }
            };
            match module_def_from_frozen(kind, name, &frozen) {
                Ok(def) => {
                    findings.extend(validate::check_module(
                        &def,
                        &frozen,
                        rel,
                        &snapshot.project.events,
                    ));
                }
                Err(err) => findings.push(Finding::error(rel, format!("{err:#}"))),
            }
        }
        // A test file has no `ModuleDef`, but it does have a shape: the `cases`
        // list `kiln test` reads.
        None if located.role == crate::loader::Role::Test => {
            if let Err(err) = testing::read_cases(&frozen) {
                findings.push(Finding::error(rel, format!("{err:#}")));
            }
        }
        None => {}
    }

    findings
        .iter()
        .map(|finding| finding_message(finding, ast))
        .collect()
}

/// A [`Finding`] as a diagnostic, anchored as precisely as it can be.
fn finding_message(finding: &Finding, ast: &AstModule) -> EvalMessage {
    // Findings from the shape checks name the binding they are about in
    // backticks and carry no span, because they were read off a frozen module
    // rather than a syntax tree. Recovering the binding's own span turns most of
    // them from a whole-file complaint into a precise one; the rest stay at the
    // top of the file, which is honest for something like a missing `handle`.
    let span = finding
        .span
        .or_else(|| quoted_name(&finding.message).and_then(|name| binding_span(ast, &name)));
    EvalMessage {
        path: finding.location.clone(),
        span,
        severity: match finding.severity {
            Severity::Error => EvalSeverity::Error,
            Severity::Warning => EvalSeverity::Warning,
        },
        name: "kiln".to_owned(),
        description: finding.message.clone(),
        full_error_with_span: None,
        original: None,
    }
}

/// The first backtick-quoted identifier in a message, if it has one.
fn quoted_name(message: &str) -> Option<String> {
    let (_, rest) = message.split_once('`')?;
    let (quoted, _) = rest.split_once('`')?;
    let is_identifier = !quoted.is_empty()
        && !quoted.starts_with(|c: char| c.is_ascii_digit())
        && quoted
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');
    is_identifier.then(|| quoted.to_owned())
}

/// Where a top-level binding of `name` is written.
fn binding_span(ast: &AstModule, name: &str) -> Option<ResolvedSpan> {
    fn scan(stmt: &AstStmt, name: &str) -> Option<starlark::codemap::Span> {
        match &stmt.node {
            StmtP::Statements(statements) => statements.iter().find_map(|inner| scan(inner, name)),
            StmtP::Assign(assign) => match &assign.lhs.node {
                AssignTargetP::Identifier(ident) if ident.ident == name => Some(ident.span),
                _ => None,
            },
            StmtP::Def(def) if def.name.ident == name => Some(def.name.span),
            _ => None,
        }
    }
    scan(ast.statement(), name).map(|span| ast.file_span(span).resolve_span())
}

/// Two tiers can describe the same problem: an undefined name is a scope error to
/// the resolve tier and an evaluation failure to the project tier, in the same
/// words at the same place.
fn dedupe(messages: &mut Vec<EvalMessage>) {
    let mut seen = Vec::new();
    messages.retain(|message| {
        let key = (message.span, message.description.clone());
        if seen.contains(&key) {
            return false;
        }
        seen.push(key);
        true
    });
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;
    use crate::lsp::env::Envs;
    use crate::lsp::project::Projects;

    const EVENTS: &str = r#"
order_placed = event(
    type = "order.placed",
    fields = {"order_id": uuid(), "shop_id": uint()},
)
"#;

    /// A project with one event module and one library, ready for a buffer to be
    /// diagnosed against.
    fn project() -> TempDir {
        let dir = TempDir::new().unwrap();
        for sub in [
            "events",
            "lib",
            "commands",
            "projectors",
            "effects",
            "tests",
        ] {
            fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        fs::write(dir.path().join("events/order.star"), EVENTS).unwrap();
        fs::write(
            dir.path().join("lib/validation.star"),
            "def is_blank(s):\n    return not s.strip()\n",
        )
        .unwrap();
        dir
    }

    /// Diagnose `src` as if it were the file at `rel`, with the project tier on.
    fn check(dir: &Path, rel: &str, src: &str) -> Vec<EvalMessage> {
        let projects = Projects::new();
        let envs = Envs::new();
        let path = dir.join(rel);
        // The file need not exist on disk: the buffer is what is diagnosed. But
        // it must be placed, and placement reads the directory convention.
        fs::write(&path, src).unwrap();
        let located = projects.locate(&path).expect("a located document");
        let env = Env::for_role(located.role);
        let snapshot = projects.snapshot(&located.root);
        let (_, messages) = diagnose(
            &located,
            src.to_owned(),
            envs.globals(env),
            env,
            Some(&snapshot),
        );
        messages
    }

    fn errors(messages: &[EvalMessage]) -> Vec<String> {
        messages
            .iter()
            .filter(|m| m.severity == EvalSeverity::Error)
            .map(|m| m.description.clone())
            .collect()
    }

    /// The regression this whole server exists for: a correct kiln module uses
    /// builtins no generic Starlark server knows, and must come back clean.
    #[test]
    fn a_correct_module_is_clean() {
        let dir = project();
        let sources = [
            (
                "commands/place-order.star",
                r#"
load("events/order.star", "order_placed")
load("lib/validation.star", "is_blank")

input = schema(order_id = uuid(), shop_id = uint(), note = str())

def query(input):
    return order_placed(shop_id = input.shop_id)

initial = {"seen": False}

fold = {order_placed(): lambda state, event: dict(state, seen = True)}

def handle(input, state):
    if is_blank(input.note):
        return reject("blank_note", "note must not be blank")
    return order_placed(order_id = input.order_id, shop_id = input.shop_id)
"#,
            ),
            (
                "projectors/orders.star",
                r#"
load("events/order.star", "order_placed")

orders = entity(
    key = "order_id",
    fields = {"order_id": uuid(), "shop_id": uint()},
    indexes = [index("by_shop", ["shop_id"])],
)

handle = {
    order_placed(): lambda event: [put(orders, {
        "order_id": event.data.order_id,
        "shop_id": event.data.shop_id,
    })],
}
"#,
            ),
            (
                "effects/notify.star",
                r#"
load("events/order.star", "order_placed")

def on_placed(event):
    log("placed at " + now())
    http.post(url = "https://example.test/hook", body = {"id": event.data.order_id})

handle = {order_placed(): on_placed}
"#,
            ),
            (
                "tests/place-order.star",
                r#"
load("events/order.star", "order_placed")

cases = [
    case(
        command = "place-order",
        input = {"order_id": "11111111-1111-1111-1111-111111111111", "shop_id": 1, "note": "hi"},
        expect = order_placed(order_id = "11111111-1111-1111-1111-111111111111", shop_id = 1),
    ),
]
"#,
            ),
        ];
        for (rel, src) in sources {
            let messages = check(dir.path(), rel, src);
            assert!(
                errors(&messages).is_empty(),
                "{rel} should be clean, got {:?}",
                errors(&messages)
            );
        }
    }

    /// The builtins are per directory, so the same source is right in one place
    /// and wrong in another. No stub-file mechanism can express that.
    #[test]
    fn a_builtin_is_undefined_outside_its_own_directory() {
        let dir = project();
        let source = r#"
load("events/order.star", "order_placed")

handle = {order_placed(): lambda event: [] if now() else []}
"#;
        // `now()` is a command's and an effect's, never a projector's.
        let messages = check(dir.path(), "projectors/clock.star", source);
        let projector_errors = errors(&messages);
        assert!(
            projector_errors.iter().any(|e| e.contains("now")),
            "expected `now` to be undefined in a projector, got {projector_errors:?}"
        );

        let messages = check(dir.path(), "effects/clock.star", source);
        assert!(
            errors(&messages).is_empty(),
            "the same source is fine in an effect, got {:?}",
            errors(&messages)
        );
    }

    /// A misspelled name should name the one that was meant. This is why scope
    /// resolution is used rather than the undefined-variable lint.
    #[test]
    fn a_misspelled_name_suggests_the_real_one() {
        let dir = project();
        let messages = check(
            dir.path(),
            "commands/typo.star",
            "input = schema(order_id = uuid())\n\ndef handle(input, state):\n    return rejcet(\"a\", \"b\")\n",
        );
        let errors = errors(&messages);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("rejcet") && e.contains("reject")),
            "expected a did-you-mean for `reject`, got {errors:?}"
        );
    }

    #[test]
    fn kilns_load_rules_are_reported_where_the_load_is_written() {
        let dir = project();
        fs::write(dir.path().join("commands/other.star"), "input = schema()\n").unwrap();

        let messages = check(
            dir.path(),
            "commands/a.star",
            "load(\"commands/other.star\", \"input\")\n\ninput = schema()\n\ndef handle(input, state):\n    return None\n",
        );
        let errors = errors(&messages);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("may only load from events/ or lib/")),
            "{errors:?}"
        );
        let load = messages
            .iter()
            .find(|m| m.description.contains("may only load"))
            .unwrap();
        assert_eq!(load.span.expect("a span").begin.line, 0);
    }

    /// A path typo is only caught at evaluation by the loader, which means after
    /// everything else is right. Here it is caught as it is typed.
    #[test]
    fn a_load_of_a_missing_file_is_reported() {
        let dir = project();
        let messages = check(
            dir.path(),
            "commands/a.star",
            "load(\"events/odrer.star\", \"order_placed\")\n\ninput = schema()\n\ndef handle(input, state):\n    return None\n",
        );
        assert!(
            errors(&messages)
                .iter()
                .any(|e| e.contains("no such file in this project")),
            "{:?}",
            errors(&messages)
        );
    }

    /// The project tier: this needs the module evaluated, not just parsed.
    #[test]
    fn a_shape_error_is_reported_against_its_own_binding() {
        let dir = project();
        let messages = check(
            dir.path(),
            "commands/a.star",
            "input = schema(order_id = uuid())\n\ninitial = lambda: 1\n\ndef handle(input, state):\n    return None\n",
        );
        let initial = messages
            .iter()
            .find(|m| m.description.contains("`initial` must be a value"))
            .unwrap_or_else(|| panic!("{:?}", errors(&messages)));
        // Recovered from the binding rather than left at the top of the file.
        assert_eq!(initial.span.expect("a span").begin.line, 2);
    }

    #[test]
    fn a_missing_handle_is_reported_for_the_file() {
        let dir = project();
        let messages = check(
            dir.path(),
            "commands/a.star",
            "input = schema(order_id = uuid())\n",
        );
        let missing = messages
            .iter()
            .find(|m| m.description.contains("missing required `handle`"))
            .unwrap_or_else(|| panic!("{:?}", errors(&messages)));
        // Nothing to point at, so it stays at the top rather than guessing.
        assert_eq!(missing.span, None);
    }

    #[test]
    fn a_syntax_error_is_the_only_thing_reported() {
        let dir = project();
        let messages = check(dir.path(), "commands/a.star", "input = schema(\n");
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert_eq!(messages[0].severity, EvalSeverity::Error);
    }

    /// Unused bindings are the product of a kiln module, not a defect: the whole
    /// file is top-level assignments nothing else in it reads.
    #[test]
    fn the_noisy_lints_are_not_reported() {
        let dir = project();
        let messages = check(
            dir.path(),
            "commands/a.star",
            "input = schema(order_id = uuid())\n\ndef handle(input, state):\n    unused = 1\n    return None\n",
        );
        assert!(messages.is_empty(), "{messages:?}");
    }

    #[test]
    fn a_file_outside_a_project_gets_a_hint_and_nothing_else() {
        let (ast, messages) = unlocated("scratch.star", "x = schema()\n".to_owned());
        assert!(ast.is_some());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].name, "kiln-not-a-module");
        assert_eq!(messages[0].severity, EvalSeverity::Advice);
    }
}
