//! Loading a project: the directory convention, and one heklang program.
//!
//! The convention is hekla's and the program is heklang's. Every `.hk` file under the
//! project is one program with no import graph and no order, so what used to be a
//! `load()` resolver is now a walk and a single `check_files`. What survives is the
//! part heklang has no opinion about: which directory a declaration may live in, and
//! that `commands/internal/` is not routed over HTTP.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use heklang::{Diagnostic, Program, Severity as HekSeverity};
use walkdir::WalkDir;

use crate::config::Config;
use crate::hash;
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

/// The directories a project's modules live in.
pub const MODULE_DIRS: [&str; 6] = [
    "events",
    "lib",
    "commands",
    "projectors",
    "effects",
    "tests",
];

/// Which directory convention a file falls under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Events,
    Lib,
    Command { internal: bool },
    Projector,
    Effect,
    Test,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Role::Events => "events",
            Role::Lib => "lib",
            Role::Command { .. } => "command",
            Role::Projector => "projector",
            Role::Effect => "effect",
            Role::Test => "test",
        }
    }

    /// The declaration kind a file in this directory may define. `None` for the
    /// directories that declare no handler.
    pub fn module_kind(self) -> Option<ModuleKind> {
        match self {
            Role::Command { .. } => Some(ModuleKind::Command),
            Role::Projector => Some(ModuleKind::Projector),
            Role::Effect => Some(ModuleKind::Effect),
            Role::Events | Role::Lib | Role::Test => None,
        }
    }
}

/// The single source of truth for the directory convention.
pub fn role_for(rel: &str) -> Option<Role> {
    let dir = rel.split('/').next()?;
    if !rel.ends_with(".hk") || rel.len() <= dir.len() + 1 {
        return None;
    }
    match dir {
        "events" => Some(Role::Events),
        "lib" => Some(Role::Lib),
        "commands" => Some(Role::Command {
            internal: rel.starts_with("commands/internal/"),
        }),
        "projectors" => Some(Role::Projector),
        "effects" => Some(Role::Effect),
        "tests" => Some(Role::Test),
        _ => None,
    }
}

/// A loaded command, plus where it came from and whether it is internal.
pub struct CommandUnit {
    pub def: ModuleDef,
    pub rel_path: String,
    pub source_hash: String,
    /// Internal commands (`commands/internal/`) are invokable by effects but not
    /// routed over HTTP.
    pub internal: bool,
}

pub struct ProjectorUnit {
    pub def: ModuleDef,
    pub rel_path: String,
    pub source_hash: String,
    /// The entities this projector declares, as tables.
    pub entities: Vec<EntityDef>,
    /// The event types its handlers select, which is its subscription.
    pub sources: Vec<String>,
}

pub struct EffectUnit {
    pub def: ModuleDef,
    pub rel_path: String,
    pub source_hash: String,
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
    pub events: EventDefs,
    pub commands: Vec<CommandUnit>,
    pub projectors: Vec<ProjectorUnit>,
    pub effects: Vec<EffectUnit>,
    pub findings: Vec<Finding>,
}

impl LoadedProject {
    pub fn load(root: &Path) -> LoadedProject {
        let mut findings = Vec::new();
        let config = match Config::load(root) {
            Ok(config) => config,
            Err(err) => {
                findings.push(Finding::error("hekla.toml", err.to_string()));
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

        let hashes: HashMap<String, String> = sources
            .iter()
            .map(|(rel, text)| (rel.clone(), hash::sha256_hex(text.as_bytes())))
            .collect();

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
        let hash_of = |module: &Option<String>| {
            module
                .as_ref()
                .and_then(|rel| hashes.get(rel))
                .cloned()
                .unwrap_or_default()
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
                source_hash: hash_of(&command.module),
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
                source_hash: hash_of(&projector.module),
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
                source_hash: hash_of(&effect.module),
                rel_path: rel,
                sources,
            });
        }

        LoadedProject {
            root: root.to_path_buf(),
            config,
            program,
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

/// Every `.hk` file under a module directory, sorted.
pub(crate) fn hek_files(root: &Path, findings: &mut Vec<Finding>) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in MODULE_DIRS {
        let start = root.join(dir);
        if !start.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&start).sort_by_file_name() {
            match entry {
                Ok(entry) if entry.file_type().is_file() => {
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "hk") {
                        found.push(path.to_path_buf());
                    }
                }
                Ok(_) => {}
                Err(err) => findings.push(Finding::error(dir, format!("walking: {err}"))),
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
    fn the_directory_convention_decides_a_files_role() {
        assert_eq!(role_for("events/order.hk"), Some(Role::Events));
        assert_eq!(role_for("lib/validation.hk"), Some(Role::Lib));
        assert_eq!(
            role_for("commands/place-order.hk"),
            Some(Role::Command { internal: false })
        );
        assert_eq!(
            role_for("commands/internal/record-welcome.hk"),
            Some(Role::Command { internal: true })
        );
        assert_eq!(role_for("projectors/orders.hk"), Some(Role::Projector));
        assert_eq!(role_for("effects/notify.hk"), Some(Role::Effect));
        assert_eq!(role_for("tests/place-order.hk"), Some(Role::Test));
    }

    #[test]
    fn a_file_outside_the_convention_has_no_role() {
        assert_eq!(role_for("README.md"), None);
        assert_eq!(role_for("events/order.star"), None, "the language changed");
        assert_eq!(role_for("scratch/thing.hk"), None);
        assert_eq!(role_for("events"), None, "a directory is not a module");
    }
}
