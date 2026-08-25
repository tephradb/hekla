//! The hekla half of the language server: what `starlark_lsp` asks, and what hekla
//! answers.
//!
//! Every method here is a place where hekla knows something a generic Starlark
//! server cannot: which builtins are in scope (the file's directory decides),
//! where a `load()` path points (the project root, under hekla's restriction), and
//! what a string literal in a particular position refers to.

use std::fs;
use std::io;
use std::path::Path;

use starlark::docs::DocModule;
use starlark_lsp::completion::{StringCompletionResult, StringCompletionType};
use starlark_lsp::error::eval_message_to_lsp_diagnostic;
use starlark_lsp::server::{LspContext, LspEvalResult, LspUri, StringLiteralResult};

use crate::loader::{is_library_path, normalize_load_path};
use crate::lsp::diagnostics;
use crate::lsp::env::{Env, Envs};
use crate::lsp::project::{Located, Projects};
use crate::lsp::stubs::stub_dialect;

pub(crate) struct HeklaContext {
    envs: Envs,
    projects: Projects,
    /// Whether to evaluate each buffer against its project. Off leaves parsing,
    /// hekla's load rules and name resolution, which is the bulk of the value at a
    /// fraction of the cost.
    project_checks: bool,
}

impl HeklaContext {
    pub(crate) fn new(project_checks: bool) -> HeklaContext {
        HeklaContext {
            envs: Envs::new(),
            projects: Projects::new(),
            project_checks,
        }
    }

    /// Place a document, for the methods that only work on real files.
    fn locate(&self, uri: &LspUri) -> Option<Located> {
        match uri {
            LspUri::File(path) => self.projects.locate(path),
            _ => None,
        }
    }
}

impl LspContext for HeklaContext {
    fn parse_file_with_contents(&self, uri: &LspUri, content: String) -> LspEvalResult {
        let (ast, messages) = match uri {
            LspUri::File(path) => match self.projects.locate(path) {
                Some(located) => {
                    let env = Env::for_role(located.role);
                    let snapshot = self
                        .project_checks
                        .then(|| self.projects.snapshot(&located.root));
                    diagnostics::diagnose(
                        &located,
                        content,
                        self.envs.globals(env),
                        env,
                        snapshot.as_deref(),
                    )
                }
                None => diagnostics::unlocated(&path.to_string_lossy(), content),
            },
            // A generated stub. Parsing it is what gives goto-definition a span to
            // land on; it is generated, so it is never diagnosed.
            LspUri::Starlark(path) => (
                starlark::syntax::AstModule::parse(
                    &path.to_string_lossy(),
                    content,
                    &stub_dialect(),
                )
                .ok(),
                Vec::new(),
            ),
            LspUri::Other(_) => return LspEvalResult::default(),
        };

        LspEvalResult {
            diagnostics: messages
                .into_iter()
                .map(eval_message_to_lsp_diagnostic)
                .collect(),
            ast,
        }
    }

    fn resolve_load(
        &self,
        path: &str,
        current_file: &LspUri,
        _workspace_root: Option<&Path>,
    ) -> Result<LspUri, String> {
        // The workspace root is ignored: hekla finds the project from the document
        // itself, which is strictly more precise when a workspace holds several.
        let located = self
            .locate(current_file)
            .ok_or_else(|| format!("`{current_file}` is not a module in a hekla project"))?;
        let normalized = normalize_load_path(path)?;
        if !is_library_path(&normalized) {
            return Err(format!(
                "load(\"{path}\") is not allowed; a {} may only load from events/ or lib/",
                located.role.label()
            ));
        }
        // Deliberately not checked for existence: a load written before the file
        // it names is not an error here, and `get_load_contents` reports a missing
        // file as absent rather than as a failure.
        Ok(LspUri::File(located.root.join(normalized)))
    }

    fn render_as_load(
        &self,
        target: &LspUri,
        current_file: &LspUri,
        _workspace_root: Option<&Path>,
    ) -> Result<String, String> {
        // Refusing is the feature, not a failure path. The server calls this for
        // every other open document to decide what to offer in completion, and
        // skips the ones that error. Refusing anything hekla could not load stops
        // it offering a symbol whose auto-inserted `load()` the loader rejects.
        let target = self
            .locate(target)
            .ok_or_else(|| format!("`{target}` is not a module in a hekla project"))?;
        let current = self
            .locate(current_file)
            .ok_or_else(|| format!("`{current_file}` is not a module in a hekla project"))?;
        if target.root != current.root {
            return Err("a module may not load from another hekla project".to_owned());
        }
        if !is_library_path(&target.rel) {
            return Err(format!(
                "a {} may not be loaded; only events/ and lib/ may be",
                target.role.label()
            ));
        }
        // Always project-root-relative. A path relative to the importing file
        // would be wrong: hekla resolves every load from the root.
        Ok(target.rel)
    }

    fn resolve_string_literal(
        &self,
        literal: &str,
        current_file: &LspUri,
        _workspace_root: Option<&Path>,
    ) -> Result<Option<StringLiteralResult>, String> {
        // Only the string shapes hekla gives meaning to. Treating every string as a
        // path, as the reference implementation does, would try to open a file for
        // an event type, a rejection code or a URL.
        //
        // This matches on the value alone, because that is all the position gives
        // us: a string that merely happens to equal a command's name jumps to that
        // command. Harmless, and worth the jumps it does get right.
        let Some(located) = self.locate(current_file) else {
            return Ok(None);
        };
        let root = &located.root;
        let candidates = [
            is_library_path(literal).then(|| literal.to_owned()),
            Some(format!("commands/{literal}.star")),
            Some(format!("commands/internal/{literal}.star")),
            Some(format!("projectors/{literal}.star")),
        ];
        for rel in candidates.into_iter().flatten() {
            let path = root.join(&rel);
            if path.is_file() {
                return Ok(Some(StringLiteralResult {
                    uri: LspUri::File(path),
                    location_finder: None,
                }));
            }
        }
        Ok(None)
    }

    fn get_load_contents(&self, uri: &LspUri) -> Result<Option<String>, String> {
        match uri {
            LspUri::File(path) => match fs::read_to_string(path) {
                Ok(source) => Ok(Some(source)),
                // Absent is not an error: a load may name a file yet to be written.
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(err) => Err(err.to_string()),
            },
            LspUri::Starlark(_) => Ok(self.envs.stub(uri).map(str::to_owned)),
            LspUri::Other(_) => Err(format!("`{uri}` is not a file or a builtin")),
        }
    }

    fn get_environment(&self, uri: &LspUri) -> DocModule {
        match self.locate(uri) {
            Some(located) => self.envs.docs(Env::for_role(located.role)).clone(),
            // Offering hekla's builtins for a file hekla would never load would be
            // the mistake this server exists to correct, in the other direction.
            None => DocModule::default(),
        }
    }

    fn get_uri_for_global_symbol(
        &self,
        current_file: &LspUri,
        symbol: &str,
    ) -> Result<Option<LspUri>, String> {
        // Scoped to the file's own environment, because the same name means
        // different things in different directories: `now` is journaled in an
        // effect and pinned in a command, and `str` is hekla's, not Starlark's.
        Ok(self
            .locate(current_file)
            .and_then(|located| self.envs.symbol_uri(Env::for_role(located.role), symbol)))
    }

    fn get_string_completion_options(
        &self,
        document_uri: &LspUri,
        kind: StringCompletionType,
        current_value: &str,
        _workspace_root: Option<&Path>,
    ) -> Result<Vec<StringCompletionResult>, String> {
        // Only inside a `load()`. Elsewhere the position is unknown, so anything
        // offered would land in rejection codes, entity keys and URLs alike.
        if kind != StringCompletionType::LoadPath {
            return Ok(Vec::new());
        }
        let Some(located) = self.locate(document_uri) else {
            return Ok(Vec::new());
        };
        // Offering exactly the loadable paths turns hekla's restriction from
        // something to trip over into the list itself.
        let snapshot = self.projects.snapshot(&located.root);
        Ok(snapshot
            .project
            .library_paths
            .iter()
            .filter(|path| path.starts_with(current_value))
            .map(|path| StringCompletionResult {
                value: path.clone(),
                insert_text: None,
                insert_text_offset: 0,
                kind: lsp_types::CompletionItemKind::FILE,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn project() -> TempDir {
        let dir = TempDir::new().unwrap();
        for sub in ["events", "lib", "commands", "projectors", "effects"] {
            fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        fs::write(
            dir.path().join("events/order.star"),
            "order_placed = event(type = \"order.placed\", fields = {\"order_id\": uuid()})\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("lib/validation.star"),
            "def ok():\n    return True\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("commands/place-order.star"),
            "input = schema()\n\ndef handle(input, state):\n    return None\n",
        )
        .unwrap();
        dir
    }

    fn uri(dir: &TempDir, rel: &str) -> LspUri {
        LspUri::File(dir.path().join(rel))
    }

    #[test]
    fn a_load_resolves_against_the_project_root() {
        let dir = project();
        let context = HeklaContext::new(true);
        let from = uri(&dir, "commands/place-order.star");

        assert_eq!(
            context.resolve_load("events/order.star", &from, None),
            Ok(LspUri::File(dir.path().join("events/order.star")))
        );
        // Not relative to the importing file, and `./` is tolerated.
        assert_eq!(
            context.resolve_load("./lib/validation.star", &from, None),
            Ok(LspUri::File(dir.path().join("lib/validation.star")))
        );
    }

    #[test]
    fn an_illegal_load_explains_the_rule() {
        let dir = project();
        let context = HeklaContext::new(true);
        let from = uri(&dir, "commands/place-order.star");

        let err = context
            .resolve_load("projectors/orders.star", &from, None)
            .unwrap_err();
        assert!(err.contains("may only load from events/ or lib/"), "{err}");

        let err = context
            .resolve_load("../escape.star", &from, None)
            .unwrap_err();
        assert!(err.contains(".."), "{err}");
    }

    /// Refusing here is what stops completion offering a symbol whose inserted
    /// `load()` the loader would reject.
    #[test]
    fn only_shared_modules_render_as_a_load() {
        let dir = project();
        let context = HeklaContext::new(true);
        let from = uri(&dir, "commands/place-order.star");

        assert_eq!(
            context.render_as_load(&uri(&dir, "events/order.star"), &from, None),
            Ok("events/order.star".to_owned())
        );
        assert!(
            context
                .render_as_load(&uri(&dir, "commands/place-order.star"), &from, None)
                .is_err()
        );
    }

    #[test]
    fn the_environment_follows_the_directory() {
        let dir = project();
        let context = HeklaContext::new(true);

        let command = context.get_environment(&uri(&dir, "commands/place-order.star"));
        assert!(command.members.contains_key("now"));
        let projector = context.get_environment(&uri(&dir, "projectors/orders.star"));
        assert!(!projector.members.contains_key("now"));
        assert!(projector.members.contains_key("get"));

        // A file hekla would never load gets nothing, rather than a guess.
        let outside = LspUri::File(dir.path().join("scratch.star"));
        assert!(context.get_environment(&outside).members.is_empty());
    }

    /// Goto-definition on a builtin: a URI, whose contents parse, containing the
    /// symbol. Hover uses a different path, so both tables must agree.
    #[test]
    fn a_builtin_resolves_to_a_stub_that_parses() {
        let dir = project();
        let context = HeklaContext::new(true);
        let from = uri(&dir, "effects/notify.star");

        let target = context
            .get_uri_for_global_symbol(&from, "http")
            .unwrap()
            .expect("a stub for http");
        let source = context
            .get_load_contents(&target)
            .unwrap()
            .expect("stub contents");
        assert!(source.contains("http = struct("), "{source}");

        // And scoping: `http` is an effect's alone.
        let from = uri(&dir, "commands/place-order.star");
        assert_eq!(context.get_uri_for_global_symbol(&from, "http"), Ok(None));
    }

    #[test]
    fn load_path_completion_offers_exactly_the_loadable_paths() {
        let dir = project();
        let context = HeklaContext::new(true);
        let from = uri(&dir, "commands/place-order.star");

        let options = context
            .get_string_completion_options(&from, StringCompletionType::LoadPath, "", None)
            .unwrap();
        let values: Vec<&str> = options.iter().map(|o| o.value.as_str()).collect();
        assert_eq!(values, ["events/order.star", "lib/validation.star"]);

        // A prefix narrows it, and a plain string offers nothing.
        let options = context
            .get_string_completion_options(&from, StringCompletionType::LoadPath, "lib/", None)
            .unwrap();
        assert_eq!(options.len(), 1);
        let options = context
            .get_string_completion_options(&from, StringCompletionType::String, "", None)
            .unwrap();
        assert!(options.is_empty());
    }

    #[test]
    fn a_missing_load_target_is_absent_rather_than_an_error() {
        let context = HeklaContext::new(true);
        let missing = LspUri::File(Path::new("/nonexistent/hekla/events/nope.star").to_path_buf());
        assert_eq!(context.get_load_contents(&missing), Ok(None));
    }
}
