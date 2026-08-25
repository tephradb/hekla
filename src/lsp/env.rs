//! The language environment: which builtins a file sees, and their documentation.
//!
//! Kiln gives each project directory a different globals set, which is the whole
//! reason a generic Starlark language server cannot serve a kiln project: it has
//! one environment for the language, and kiln has five. Everything here is built
//! once at startup and then only read, because the LSP asks for an environment on
//! every completion and every hover.

use std::collections::HashMap;
use std::path::PathBuf;

use starlark::docs::DocModule;
use starlark::environment::Globals;
use starlark_lsp::server::LspUri;

use crate::loader::Role;
use crate::lsp::stubs::render_stub;
use crate::{starlark_builtins, testing};

/// Which set of builtins a file sees. Five sets for six roles: `events/` and
/// `lib/` are pure shared code and share the base set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Env {
    Base,
    Command,
    Projector,
    Effect,
    Test,
}

impl Env {
    pub(crate) const ALL: [Env; 5] = [
        Env::Base,
        Env::Command,
        Env::Projector,
        Env::Effect,
        Env::Test,
    ];

    /// The environment a role is evaluated in.
    ///
    /// Exhaustive on purpose: a new [`Role`] must decide what it can see here,
    /// rather than silently inheriting a set that would let the editor accept code
    /// the loader rejects.
    pub(crate) fn for_role(role: Role) -> Env {
        match role {
            Role::Events | Role::Lib => Env::Base,
            Role::Command { .. } => Env::Command,
            Role::Projector => Env::Projector,
            Role::Effect => Env::Effect,
            Role::Test => Env::Test,
        }
    }

    /// Whether a module in this environment names query clauses at top level.
    /// Mirrors the loader's choice, so a buffer evaluates the way it will deploy.
    pub(crate) fn query_mode(self) -> bool {
        match self {
            Env::Command | Env::Projector | Env::Effect => true,
            // Shared modules define events rather than filtering on them, and a
            // test file constructs them to seed a store.
            Env::Base | Env::Test => false,
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Env::Base => "base",
            Env::Command => "command",
            Env::Projector => "projector",
            Env::Effect => "effect",
            Env::Test => "test",
        }
    }

    fn index(self) -> usize {
        match self {
            Env::Base => 0,
            Env::Command => 1,
            Env::Projector => 2,
            Env::Effect => 3,
            Env::Test => 4,
        }
    }

    fn globals(self) -> Globals {
        match self {
            Env::Base => starlark_builtins::globals(),
            Env::Command => starlark_builtins::command_globals(),
            Env::Projector => starlark_builtins::projector_globals(),
            Env::Effect => starlark_builtins::effect_globals(),
            Env::Test => testing::test_globals(),
        }
    }
}

/// One environment, with everything the server serves from it precomputed.
struct Entry {
    globals: Globals,
    /// Prebuilt: `LspContext::get_environment` returns a `DocModule` by value on
    /// every completion request and every hover of an unresolved name, so the
    /// clone is forced, but rebuilding it from the frozen values is not.
    docs: DocModule,
    symbols: HashMap<String, LspUri>,
}

/// Every environment, plus the synthetic stub sources goto-definition jumps into.
pub(crate) struct Envs {
    entries: [Entry; 5],
    /// Keyed by the `starlark:` path rather than the whole URI, because that is
    /// the only kind of URI a stub ever has, and saying so keeps a `file://` from
    /// ever matching one.
    stubs: HashMap<PathBuf, String>,
}

impl Envs {
    pub(crate) fn new() -> Envs {
        let mut stubs = HashMap::new();
        let entries = Env::ALL.map(|env| {
            let globals = env.globals();
            let docs = globals.documentation();

            let mut symbols = HashMap::new();
            for (name, item) in &docs.members {
                let uri = stub_uri(env, name);
                stubs.insert(stub_path(env, name), render_stub(name, item));
                symbols.insert(name.clone(), uri);
            }

            Entry {
                globals,
                docs,
                symbols,
            }
        });
        Envs { entries, stubs }
    }

    pub(crate) fn globals(&self, env: Env) -> &Globals {
        &self.entries[env.index()].globals
    }

    pub(crate) fn docs(&self, env: Env) -> &DocModule {
        &self.entries[env.index()].docs
    }

    /// Where goto-definition should send a reader for a global. Scoped to the
    /// environment because the same name can mean different things: `now` exists
    /// for commands and for effects with different guarantees, and `str` is kiln's
    /// rather than Starlark's.
    pub(crate) fn symbol_uri(&self, env: Env, symbol: &str) -> Option<LspUri> {
        self.entries[env.index()].symbols.get(symbol).cloned()
    }

    pub(crate) fn stub(&self, uri: &LspUri) -> Option<&str> {
        match uri {
            LspUri::Starlark(path) => self.stubs.get(path).map(String::as_str),
            _ => None,
        }
    }
}

/// The path a builtin's stub lives at.
///
/// The leading slash is required: `LspUri::try_from` rejects a `starlark:` URI
/// whose path does not start with one.
fn stub_path(env: Env, symbol: &str) -> PathBuf {
    PathBuf::from(format!("/kiln/{}/{symbol}.star", env.slug()))
}

/// The synthetic URI for one builtin. Built as the variant directly rather than
/// by parsing a formatted string, so the invariant above cannot be lost.
fn stub_uri(env: Env, symbol: &str) -> LspUri {
    LspUri::Starlark(stub_path(env, symbol))
}

#[cfg(test)]
mod tests {
    use starlark::docs::DocItem;
    use starlark::syntax::AstModule;
    use starlark_lsp::server::LspUri;

    use super::*;
    use crate::lsp::stubs::stub_dialect;

    #[test]
    fn each_role_sees_the_builtins_its_directory_gets() {
        let envs = Envs::new();
        let has = |env: Env, name: &str| envs.docs(env).members.contains_key(name);

        // The clock is a command's and an effect's, never a projector's.
        assert!(has(Env::Command, "now"));
        assert!(has(Env::Effect, "now"));
        assert!(!has(Env::Projector, "now"));
        assert!(!has(Env::Base, "now"));

        // Reading a read model is a projector's, and `http` an effect's.
        assert!(has(Env::Projector, "get"));
        assert!(!has(Env::Command, "get"));
        assert!(has(Env::Effect, "http"));
        assert!(!has(Env::Command, "http"));

        // `case(...)` belongs to test files alone.
        assert!(has(Env::Test, "case"));
        assert!(!has(Env::Command, "case"));

        // And the shared vocabulary is everywhere.
        for env in Env::ALL {
            assert!(has(env, "schema"), "{env:?} is missing schema");
            assert!(has(env, "event"), "{env:?} is missing event");
            assert!(has(env, "reject"), "{env:?} is missing reject");
        }
    }

    #[test]
    fn the_http_namespace_stays_a_namespace() {
        let envs = Envs::new();
        let http = envs.docs(Env::Effect).members.get("http").expect("http");
        // Not flattened into the top level: a namespace documents as a module, and
        // hover renders its members from that.
        let DocItem::Module(module) = http else {
            panic!("expected http to document as a namespace, got {http:?}");
        };
        for verb in ["get", "post", "put", "delete", "patch"] {
            assert!(module.members.contains_key(verb), "http.{verb} is missing");
        }
    }

    /// Kiln shadows `str`, `int` and `bool`. The proof the shadow wins is that the
    /// documentation carries kiln's field options rather than the stdlib's.
    #[test]
    fn the_shadowed_scalars_document_as_kilns() {
        let envs = Envs::new();
        for (name, option) in [
            ("str", "max_length"),
            ("int", "indexed"),
            ("bool", "subject"),
        ] {
            let item = envs.docs(Env::Base).members.get(name).expect(name);
            let DocItem::Member(starlark::docs::DocMember::Function(function)) = item else {
                panic!("expected {name} to be a function, got {item:?}");
            };
            assert!(
                function
                    .params
                    .regular_params()
                    .any(|param| param.name == option),
                "{name} has no `{option}` parameter, so the standard builtin won"
            );
        }
    }

    /// Both non-obvious ways a stub can fail silently, over every symbol at once:
    /// a URI that does not survive the round trip through `lsp_types::Uri` never
    /// reaches the client, and a stub that does not parse has no span to jump to.
    #[test]
    fn every_builtin_has_a_reachable_parseable_stub() {
        let envs = Envs::new();
        for env in Env::ALL {
            for name in envs.docs(env).members.keys() {
                let uri = envs
                    .symbol_uri(env, name)
                    .unwrap_or_else(|| panic!("{env:?}: no stub URI for `{name}`"));

                let raw: lsp_types::Uri = (&uri)
                    .try_into()
                    .unwrap_or_else(|err| panic!("{env:?}/{name}: URI is unrenderable: {err}"));
                let back = LspUri::try_from(raw)
                    .unwrap_or_else(|err| panic!("{env:?}/{name}: URI does not round-trip: {err}"));
                assert_eq!(back, uri, "{env:?}/{name}: URI changed in the round trip");

                let source = envs
                    .stub(&uri)
                    .unwrap_or_else(|| panic!("{env:?}: no stub source for `{name}`"));
                let ast =
                    AstModule::parse(&format!("{name}.star"), source.to_owned(), &stub_dialect())
                        .unwrap_or_else(|err| {
                            panic!("{env:?}/{name}: stub does not parse: {err}\n---\n{source}")
                        });

                // A stub with no top-level binding of that name would leave
                // goto-definition pointing at line 0 rather than the definition.
                assert!(
                    source.contains(&format!("def {name}("))
                        || source.contains(&format!("{name} =")),
                    "{env:?}/{name}: stub binds no `{name}`\n---\n{source}"
                );
                let _ = ast;
            }
        }
    }
}
