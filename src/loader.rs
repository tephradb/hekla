//! Loading a project: the directory convention, and one heklang program.
//!
//! The convention is hekla's and the program is heklang's. Every `.hk` file under the
//! project is one program with no import graph and no order, so what used to be a
//! `load()` resolver is now a walk and a single `check_files`.
//!
//! What survives is the part heklang has no opinion about, and it is three directories
//! rather than a layout: a directory is enforced when a declaration in the wrong one
//! would change what the runtime does, and only `commands/` (routed, and
//! `commands/internal/` not), `projectors/` and `effects/` clear that bar. Where an
//! event, a guard, a refusal or a `fn` is declared is the author's business. `events/`,
//! `lib/` and `tests/` are what the examples do, and nothing here checks them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use heklang::{Diagnostic, Digest, Entry, Kind, Program, Severity as HekSeverity};
use walkdir::WalkDir;

use crate::config::Config;
use crate::schema::{self, EntityDef, EventDef, EventDefs, InputSchema, ModuleDef, ModuleKind};

/// How severe a [`Finding`] is. Only errors fail `hek check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// Where in a file a finding is. Line and column are 0-based, which is what an editor
/// and the CLI's `+1` rendering both expect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub line: u32,
    pub column: u32,
}

/// One problem found while loading or checking a project.
#[derive(Debug, Clone)]
pub struct Finding {
    pub severity: Severity,
    /// The project-relative path (or config file name) the finding concerns.
    pub location: String,
    pub message: String,
    /// Where in `location` the problem is. Findings about a file as a whole leave it
    /// `None`.
    pub span: Option<Span>,
    /// What to do about it, when the checker had something to say. heklang carries a
    /// hint on many of its diagnostics and dropping it would lose the better half of
    /// the message.
    pub hint: Option<String>,
}

impl Finding {
    pub fn error(location: impl Into<String>, message: impl Into<String>) -> Finding {
        Finding {
            severity: Severity::Error,
            location: location.into(),
            message: message.into(),
            span: None,
            hint: None,
        }
    }

    pub fn warning(location: impl Into<String>, message: impl Into<String>) -> Finding {
        Finding {
            severity: Severity::Warning,
            location: location.into(),
            message: message.into(),
            span: None,
            hint: None,
        }
    }

    /// Anchor this finding to a source span.
    pub fn with_span(mut self, span: Span) -> Finding {
        self.span = Some(span);
        self
    }

    /// One heklang diagnostic, which already knows its file, its extent and usually
    /// what to do about it.
    pub fn from_diagnostic(diagnostic: &Diagnostic) -> Finding {
        Finding {
            severity: match diagnostic.severity {
                HekSeverity::Error => Severity::Error,
                HekSeverity::Warning => Severity::Warning,
            },
            location: diagnostic.file.clone().unwrap_or_default(),
            message: diagnostic.message.clone(),
            span: Some(Span {
                line: diagnostic.span.start.line,
                column: diagnostic.span.start.col,
            }),
            hint: diagnostic.hint.clone(),
        }
    }
}

/// A directory that decides something, and what it decides.
///
/// There are three, and the test each one passes is that a declaration in the wrong
/// place would change what the runtime does: a command's directory is what routes it and
/// what keeps `commands/internal/` off the HTTP surface, and a projector's and an
/// effect's is what they are. No other declaration has an answer to "and then what
/// breaks", so no other directory is a rule. There is deliberately no variant for
/// `events/`, `lib/` or `tests/`: naming them here would mean enforcing them, and
/// enforcing where an event may be declared buys nothing a reader does not already get
/// from the project checking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Command { internal: bool },
    Projector,
    Effect,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Role::Command { .. } => "command",
            Role::Projector => "projector",
            Role::Effect => "effect",
        }
    }

    /// The declaration kind a file in this directory may define. Total rather than
    /// optional, which is the whole of the distinction this type now draws.
    pub fn module_kind(self) -> ModuleKind {
        match self {
            Role::Command { .. } => ModuleKind::Command,
            Role::Projector => ModuleKind::Projector,
            Role::Effect => ModuleKind::Effect,
        }
    }
}

/// The single source of truth for the directory convention. `None` is the ordinary
/// answer: the file is read like every other and no rule applies to it.
pub fn role_for(rel: &str) -> Option<Role> {
    let dir = rel.split('/').next()?;
    if !rel.ends_with(".hk") || rel.len() <= dir.len() + 1 {
        return None;
    }
    match dir {
        "commands" => Some(Role::Command {
            internal: rel.starts_with("commands/internal/"),
        }),
        "projectors" => Some(Role::Projector),
        "effects" => Some(Role::Effect),
        _ => None,
    }
}

/// A loaded command, plus where it came from and whether it is internal.
pub struct CommandUnit {
    pub def: ModuleDef,
    pub rel_path: String,
    /// This declaration's [`heklang::Digest`] entry hash: what it *does*, not what it
    /// was written as: a reformat leaves it where it was.
    pub digest_hash: String,
    /// Internal commands (`commands/internal/`) are invokable by effects but not
    /// routed over HTTP.
    pub internal: bool,
}

pub struct ProjectorUnit {
    pub def: ModuleDef,
    pub rel_path: String,
    /// See [`CommandUnit::digest_hash`]. This is also what the read model records as
    /// its definition, so a rebuild follows a change in behaviour rather than layout.
    pub digest_hash: String,
    /// The entities this projector declares, as tables.
    pub entities: Vec<EntityDef>,
    /// The event types its handlers select, which is its subscription.
    pub sources: Vec<String>,
}

pub struct EffectUnit {
    pub def: ModuleDef,
    pub rel_path: String,
    /// See [`CommandUnit::digest_hash`]. This is also what an invocation records as its
    /// `script_hash`, so a reformat no longer costs the replay check its coverage.
    pub digest_hash: String,
    /// The event types its arms select.
    pub sources: Vec<String>,
}

/// Everything the loader produced from a project directory.
pub struct LoadedProject {
    pub root: PathBuf,
    pub config: Config,
    /// The one program every declaration lives in. Parsed once and shared: heklang's
    /// `Program` is `Send + Sync`, so every thread reads this one.
    pub program: Program,
    /// What the program does, per declaration, as heklang renders it. The one source of
    /// every hash hekla records: a module hash, a projector's definition and an
    /// invocation's `script_hash` are all an entry hash out of here.
    pub digest: Digest,
    pub events: EventDefs,
    pub commands: Vec<CommandUnit>,
    pub projectors: Vec<ProjectorUnit>,
    pub effects: Vec<EffectUnit>,
    pub findings: Vec<Finding>,
}

/// Every declaration's entry hash, keyed by kind and the name heklang knows it by.
///
/// Only the three module kinds are looked up through this; the rest of the digest is
/// recorded whole. `Hash` renders as hex and does not read back, so a hash is a
/// `String` from here on and every comparison against one is a string comparison.
/// Where each declaration was written, keyed the way the digest names it.
///
/// A digest has no per-module identity on purpose: heklang treats a module as a label
/// for a diagnostic, so moving a declaration between files must not move its hash.
/// hekla still wants to show an author where something is declared, so the path rides
/// alongside the entry rather than inside it.
///
/// An event and an enum are absent because their declarations carry no module at all,
/// which is why the column this feeds is nullable.
pub fn module_paths(program: &Program) -> HashMap<(Kind, String), String> {
    let mut out = HashMap::new();
    let mut add = |kind: Kind, name: &str, module: &Option<String>| {
        if let Some(module) = module {
            out.insert((kind, name.to_owned()), module.clone());
        }
    };
    for def in &program.records {
        add(Kind::Record, &def.name, &def.module);
    }
    for def in &program.functions {
        add(Kind::Function, &def.name, &def.module);
    }
    for def in &program.commands {
        add(Kind::Command, &def.name, &def.module);
    }
    for def in &program.projectors {
        add(Kind::Projector, &def.name, &def.module);
    }
    for def in &program.effects {
        add(Kind::Effect, &def.name, &def.module);
    }
    out
}

fn digest_hashes(digest: &Digest) -> HashMap<(Kind, &str), String> {
    digest
        .entries()
        .iter()
        .map(|entry: &Entry| ((entry.kind, entry.name.as_str()), entry.hash.to_string()))
        .collect()
}

impl LoadedProject {
    pub fn load(root: &Path) -> LoadedProject {
        let mut findings = Vec::new();
        let config = match Config::load(root) {
            Ok(config) => config,
            Err(err) => {
                // `{err:#}` rather than `{err}`: the context is "parsing <path>" and the
                // cause is what is actually wrong with the file.
                findings.push(Finding::error("hekla.toml", format!("{err:#}")));
                Config::default()
            }
        };

        let mut sources: Vec<(String, String)> = Vec::new();
        for path in hek_files(root, &mut findings) {
            let rel = rel_to_string(root, &path);
            match std::fs::read_to_string(&path) {
                Ok(text) => sources.push((rel, text)),
                Err(err) => findings.push(Finding::error(rel, format!("reading: {err}"))),
            }
        }
        sources.sort_by(|left, right| left.0.cmp(&right.0));

        let borrowed: Vec<(&str, &str)> = sources
            .iter()
            .map(|(rel, text)| (rel.as_str(), text.as_str()))
            .collect();

        // Every mistake rather than only the first: `check_files` reports a pass at a
        // time, which is what lets one bad declaration not hide the rest.
        let program = match heklang::check_files(borrowed) {
            Ok(program) => program,
            Err(diagnostics) => {
                findings.extend(diagnostics.iter().map(Finding::from_diagnostic));
                return LoadedProject {
                    root: root.to_path_buf(),
                    config,
                    program: Program::default(),
                    // A program that did not check has no digest form, so there is
                    // nothing to take one of. `Digest` has no `Default`; an empty
                    // program's digest is the same empty answer.
                    digest: Digest::of(&Program::default()),
                    events: EventDefs::new(),
                    commands: Vec::new(),
                    projectors: Vec::new(),
                    effects: Vec::new(),
                    findings,
                };
            }
        };

        let events: EventDefs = EventDef::all(&program)
            .into_iter()
            .map(|def| (def.event_type.clone(), def))
            .collect();

        let defs = heklang::Defs::of(&program);
        let digest = Digest::of(&program);
        let hashes = digest_hashes(&digest);
        let hash_of = |kind: Kind, name: &str| {
            hashes
                .get(&(kind, name))
                .cloned()
                .unwrap_or_else(|| unreachable!("`{name}` checked but has no digest entry"))
        };

        let mut commands = Vec::new();
        for command in &program.commands {
            let rel = command.module.clone().unwrap_or_default();
            let role = role_for(&rel);
            if !matches!(role, Some(Role::Command { .. })) {
                findings.push(Finding::error(
                    rel.clone(),
                    format!(
                        "`command {}` must be declared under commands/",
                        command.name
                    ),
                ));
                continue;
            }
            commands.push(CommandUnit {
                def: ModuleDef::Command {
                    name: command.name.clone(),
                    input: InputSchema::of(command, defs),
                },
                internal: matches!(role, Some(Role::Command { internal: true })),
                digest_hash: hash_of(Kind::Command, &command.name),
                rel_path: rel,
            });
        }

        let mut projectors = Vec::new();
        for projector in &program.projectors {
            let rel = projector.module.clone().unwrap_or_default();
            if role_for(&rel) != Some(Role::Projector) {
                findings.push(Finding::error(
                    rel.clone(),
                    format!(
                        "`projector {}` must be declared under projectors/",
                        projector.name
                    ),
                ));
                continue;
            }
            let entities = EntityDef::all(&program, projector);
            let sources = event_types(projector.handlers.iter().map(|handler| &handler.event));
            for entity in &entities {
                if let Err(err) = entity.validate() {
                    findings.push(Finding::error(rel.clone(), err.to_string()));
                }
            }
            projectors.push(ProjectorUnit {
                def: ModuleDef::Projector {
                    name: projector.name.clone(),
                    entities: entities.clone(),
                    sources: sources.clone(),
                },
                digest_hash: hash_of(Kind::Projector, &projector.name),
                rel_path: rel,
                entities,
                sources,
            });
        }

        let mut effects = Vec::new();
        for effect in &program.effects {
            let rel = effect.module.clone().unwrap_or_default();
            if role_for(&rel) != Some(Role::Effect) {
                findings.push(Finding::error(
                    rel.clone(),
                    format!("`effect {}` must be declared under effects/", effect.name),
                ));
                continue;
            }
            let sources = event_types(effect.arms.iter().flat_map(|arm| arm.events.iter()));
            effects.push(EffectUnit {
                def: ModuleDef::Effect {
                    name: effect.name.clone(),
                    sources: sources.clone(),
                },
                digest_hash: hash_of(Kind::Effect, &effect.name),
                rel_path: rel,
                sources,
            });
        }

        LoadedProject {
            root: root.to_path_buf(),
            config,
            program,
            digest,
            events,
            commands,
            projectors,
            effects,
            findings,
        }
    }

    pub fn has_errors(&self) -> bool {
        self.findings
            .iter()
            .any(|finding| finding.severity == Severity::Error)
    }
}

/// The distinct wire event types a set of paths names, in first-seen order.
fn event_types<'a>(paths: impl Iterator<Item = &'a heklang::ir::EventPath>) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for path in paths {
        let ty = schema::event_type(path);
        if !seen.contains(&ty) {
            seen.push(ty);
        }
    }
    seen
}

/// Every `.hk` file in the project, sorted.
///
/// The whole tree, not a list of directories. heklang has no import, so a file is either
/// in the program or absent from it, and a whitelist of directories can only ever decide
/// the second one silently: what the author sees is not the file that was dropped but
/// every *use* of what it declared, failing to resolve in files that are themselves fine.
/// The convention is enforced per declaration instead, in [`role_for`], where the file
/// having been read is what makes the diagnostic possible at all.
///
/// Three directories are skipped. Hidden ones and `target` for the reason `hek` skips
/// them, which is that a build directory holding a copied `.hk` is not a second module
/// and would collide with the source it was copied from. `data/` because it is the
/// runtime's own: it holds no `.hk` at all, and on a project that has been serving for a
/// while it holds a great many other files.
pub(crate) fn hek_files(root: &Path, findings: &mut Vec<Finding>) -> Vec<PathBuf> {
    let mut found = Vec::new();
    // A root that is not a directory discovers nothing and says nothing, which is what
    // every subcommand but `openapi` wants; that one checks the path itself first.
    if !root.is_dir() {
        return found;
    }
    let walk = WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            if name.starts_with('.') || name == "target" {
                return false;
            }
            !(entry.depth() == 1 && name == "data" && entry.file_type().is_dir())
        });
    for entry in walk {
        match entry {
            Ok(entry) if entry.file_type().is_file() => {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "hk") {
                    found.push(path.to_path_buf());
                }
            }
            Ok(_) => {}
            Err(err) => {
                let location = err
                    .path()
                    .map(|path| rel_to_string(root, path))
                    .unwrap_or_default();
                findings.push(Finding::error(location, format!("walking: {err}")));
            }
        }
    }
    found
}

pub(crate) fn rel_to_string(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_enforced_directories_decide_a_files_role() {
        assert_eq!(
            role_for("commands/place-order.hk"),
            Some(Role::Command { internal: false })
        );
        assert_eq!(
            role_for("commands/internal/record-welcome.hk"),
            Some(Role::Command { internal: true })
        );
        assert_eq!(
            role_for("commands/billing/refund.hk"),
            Some(Role::Command { internal: false }),
            "only the literal `internal/` prefix marks a command internal"
        );
        assert_eq!(role_for("projectors/orders.hk"), Some(Role::Projector));
        assert_eq!(role_for("effects/notify.hk"), Some(Role::Effect));
    }

    /// Every other directory is read the same and ruled on by nothing, which is what
    /// leaves an author free to put a guard beside the one command that names it or in
    /// a file of its own.
    #[test]
    fn a_directory_that_decides_nothing_has_no_role() {
        assert_eq!(role_for("events/order.hk"), None);
        assert_eq!(role_for("lib/validation.hk"), None);
        assert_eq!(role_for("guards/shop.hk"), None);
        assert_eq!(role_for("tests/place-order.hk"), None);
        assert_eq!(role_for("scratch/thing.hk"), None);
        assert_eq!(role_for("README.md"), None);
        assert_eq!(
            role_for("commands/place-order.star"),
            None,
            "the language changed"
        );
        assert_eq!(role_for("commands"), None, "a directory is not a module");
    }

    /// Two effects in one file, so a per-file hash could not tell them apart.
    const TWO_EFFECTS: &str = r#"
effect Alpha {
  on @e.one as one {
    let reply = http.post("https://example.test/alpha", { "id": one.id })
    if reply.status >= 400 { fail("alpha rejected") }
  }
}

effect Beta {
  on @e.two as two {
    let reply = http.post("https://example.test/beta", { "id": two.id })
    if reply.status >= 400 { fail("beta rejected") }
  }
}
"#;

    fn write_project(dir: &Path, effects: &str) -> LoadedProject {
        for (rel, text) in [
            (
                "events/e.hk",
                "event @e.one { id: Uuid }\nevent @e.two { id: Uuid }\n",
            ),
            (
                "commands/emit-one.hk",
                "command EmitOne(id: Uuid) {\n  emit @e.one { id }\n}\n",
            ),
            ("effects/both.hk", effects),
        ] {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let project = LoadedProject::load(dir);
        assert!(!project.has_errors(), "{:?}", project.findings);
        project
    }

    fn effect_hash(project: &LoadedProject, name: &str) -> String {
        project
            .effects
            .iter()
            .find(|unit| unit.def.name() == name)
            .unwrap()
            .digest_hash
            .clone()
    }

    /// The two properties the digest buys, in one test because they are one claim:
    /// a hash tracks what a declaration does, and it does so per declaration.
    ///
    /// Under the per-file source hash this replaced, both halves failed. Reformatting
    /// the file moved both effects' hashes, and editing `Alpha` moved `Beta`'s too,
    /// because the hash was of the bytes the two happened to share a file with.
    #[test]
    fn a_hash_follows_one_declaration_and_only_what_it_does() {
        let dir = tempfile::tempdir().unwrap();
        let before = write_project(dir.path(), TWO_EFFECTS);
        let (alpha, beta) = (effect_hash(&before, "Alpha"), effect_hash(&before, "Beta"));

        // Reindented, commented, and with `Alpha`'s bindings renamed. A binder's name
        // never leaves the program, so none of this is observable.
        let cosmetic = r#"
// Two unrelated effects that happen to share a file.
effect Alpha {

  on @e.one as triggering {
    // Tell the alpha service.
    let response  =  http.post("https://example.test/alpha", { "id": triggering.id })
    if response.status >= 400 {
      fail("alpha rejected")
    }
  }
}

effect Beta {
  on @e.two as two {
    let reply = http.post("https://example.test/beta", { "id": two.id })
    if reply.status >= 400 { fail("beta rejected") }
  }
}
"#;
        let after = write_project(dir.path(), cosmetic);
        assert_eq!(
            effect_hash(&after, "Alpha"),
            alpha,
            "a reformat is not a change"
        );
        assert_eq!(effect_hash(&after, "Beta"), beta);

        // Now a real one, to `Alpha` only: a different URL is a different call, and the
        // journal is keyed on it.
        let edited = write_project(
            dir.path(),
            &TWO_EFFECTS.replace("example.test/alpha", "example.test/alpha-v2"),
        );
        assert_ne!(
            effect_hash(&edited, "Alpha"),
            alpha,
            "a changed call is a change"
        );
        assert_eq!(
            effect_hash(&edited, "Beta"),
            beta,
            "and it belongs to the declaration that made it, not to the file it sits in"
        );
    }

    /// An event has no unit of its own, so before the digest hekla recorded nothing
    /// about one. Its entry is what makes a later schema-drift check possible at all.
    #[test]
    fn every_declaration_has_an_entry_and_a_test_has_none() {
        let dir = tempfile::tempdir().unwrap();
        let project = write_project(dir.path(), TWO_EFFECTS);
        let named: Vec<(Kind, &str)> = project
            .digest
            .entries()
            .iter()
            .map(|entry| (entry.kind, entry.name.as_str()))
            .collect();

        assert!(named.contains(&(Kind::Event, "@e.one")));
        assert!(named.contains(&(Kind::Command, "EmitOne")));
        assert!(named.contains(&(Kind::Effect, "Alpha")));
        assert!(
            !named.iter().any(|(kind, _)| *kind == Kind::Test),
            "`entries` holds tests back, which is why nothing filters them downstream"
        );
    }
}
