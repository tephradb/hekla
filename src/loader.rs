//! The project loader: directory convention, `load()` resolution, event
//! registry.
//!
//! A kiln project is a directory tree. Kind comes from the directory
//! (`commands/`, `projectors/`, `effects/`), name from the file stem, and shared
//! code lives in `events/` and `lib/`. This module walks that tree, resolves
//! each file's `load()` imports (restricted to `events/` and `lib/`), evaluates
//! the library modules in dependency order so a shared file evaluates once, and
//! reads every command, projector and effect into a [`ModuleDef`].
//!
//! Loading is resilient: rather than bail on the first bad file, it collects
//! [`Finding`]s so `kiln check` can report every problem in one pass. The
//! semantic checks that need the event registry (query tags, source types) live
//! in [`crate::validate`], which runs on the [`LoadedProject`] this produces.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use starlark::environment::{FrozenModule, Globals};
use starlark::eval::FileLoader;
use starlark::syntax::AstModule;
use starlark::values::ValueLike;
use walkdir::WalkDir;

use crate::config::{self, Config};
use crate::starlark_builtins::{
    self, EventDef, LoadedModule, ModuleKind, eval_frozen, module_def_from_frozen,
    module_name_from_path, parse_module,
};

/// How severe a [`Finding`] is. Only errors fail `kiln check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// One problem found while loading or checking a project.
#[derive(Debug, Clone)]
pub struct Finding {
    pub severity: Severity,
    /// The project-relative path (or config file name) the finding concerns.
    pub location: String,
    pub message: String,
}

impl Finding {
    pub fn error(location: impl Into<String>, message: impl Into<String>) -> Finding {
        Finding {
            severity: Severity::Error,
            location: location.into(),
            message: message.into(),
        }
    }

    pub fn warning(location: impl Into<String>, message: impl Into<String>) -> Finding {
        Finding {
            severity: Severity::Warning,
            location: location.into(),
            message: message.into(),
        }
    }
}

/// The event definitions declared across `events/`, plus the evaluated library
/// modules kept as `load()` targets (the evaluated-module cache).
pub struct EventRegistry {
    /// Event type string to its definition.
    pub by_type: HashMap<String, EventDef>,
    /// Evaluated `events/` and `lib/` modules, keyed by their load path.
    pub library: HashMap<String, FrozenModule>,
}

/// A loaded command, plus where it came from and whether it is internal.
pub struct CommandUnit {
    pub loaded: LoadedModule,
    pub rel_path: String,
    /// Internal commands (`commands/internal/`) are invokable by effects but not
    /// routed over HTTP.
    pub internal: bool,
}

pub struct ProjectorUnit {
    pub loaded: LoadedModule,
    pub rel_path: String,
}

pub struct EffectUnit {
    pub loaded: LoadedModule,
    pub rel_path: String,
}

/// Everything the loader produced from a project directory.
pub struct LoadedProject {
    pub root: PathBuf,
    pub config: Config,
    pub events: EventRegistry,
    pub commands: Vec<CommandUnit>,
    pub projectors: Vec<ProjectorUnit>,
    pub effects: Vec<EffectUnit>,
    /// Problems found while loading. `kiln check` adds semantic findings on top.
    pub findings: Vec<Finding>,
}

impl LoadedProject {
    /// Load a project from `root`, collecting findings rather than bailing.
    pub fn load(root: &Path) -> LoadedProject {
        let mut findings = Vec::new();

        let config = match Config::load(root) {
            Ok(config) => config,
            Err(err) => {
                findings.push(Finding::error(config::FILE_NAME, format!("{err:#}")));
                Config::default()
            }
        };

        let base = starlark_builtins::globals();
        let command = starlark_builtins::command_globals();
        let projector = starlark_builtins::projector_globals();
        let effect = starlark_builtins::effect_globals();
        let mut parsed = discover_and_parse(root, &mut findings);

        // Libraries (`events/`, `lib/`) are pure, so they evaluate against the
        // base globals regardless of who imports them.
        let mut cache: HashMap<String, FrozenModule> = HashMap::new();
        let mut collector = EventCollector::default();
        evaluate_libraries(
            &mut parsed,
            &base,
            &mut cache,
            &mut collector,
            &mut findings,
        );

        let (commands, projectors, effects) = evaluate_units(
            &mut parsed,
            &command,
            &projector,
            &effect,
            &cache,
            &mut findings,
        );

        check_name_collisions(&commands, &projectors, &effects, &mut findings);

        LoadedProject {
            root: root.to_path_buf(),
            config,
            events: EventRegistry {
                by_type: collector.by_type,
                library: cache,
            },
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

    /// Evaluate an extra module (a `kiln test` file) against this project's
    /// library cache, so it can `load()` from `events/` and `lib/` exactly as a
    /// command would. The module is frozen against `globals` (the test globals).
    pub fn eval_against_libraries(
        &self,
        filename: &str,
        src: String,
        globals: &Globals,
    ) -> anyhow::Result<FrozenModule> {
        let ast = parse_module(filename, src).map_err(|err| anyhow::anyhow!("{err}"))?;
        let loader = LibraryLoader {
            cache: &self.events.library,
        };
        eval_frozen(ast, globals, Some(&loader)).map_err(|err| anyhow::anyhow!("{err}"))
    }
}

/// Which directory convention a file falls under. `Command` also records whether
/// the file sits in `commands/internal/`.
#[derive(Debug, Clone, Copy)]
enum Role {
    Events,
    Lib,
    Command { internal: bool },
    Projector,
    Effect,
}

impl Role {
    fn label(self) -> &'static str {
        match self {
            Role::Events => "event module",
            Role::Lib => "library module",
            Role::Command { .. } => "command",
            Role::Projector => "projector",
            Role::Effect => "effect",
        }
    }

    /// The module kind for a command, projector or effect; `None` for library
    /// files, which have no `ModuleDef`.
    fn module_kind(self) -> Option<ModuleKind> {
        match self {
            Role::Command { .. } => Some(ModuleKind::Command),
            Role::Projector => Some(ModuleKind::Projector),
            Role::Effect => Some(ModuleKind::Effect),
            Role::Events | Role::Lib => None,
        }
    }
}

/// A parsed-but-not-yet-evaluated file. `ast` is `None` when reading or parsing
/// failed; the load edges are captured up front so evaluation order can be
/// resolved before any module is consumed by evaluation.
struct ParsedFile {
    role: Role,
    rel_path: String,
    /// The load path (cache key) for library files; `None` otherwise.
    load_path: Option<String>,
    /// The slug name for command/projector/effect files; `None` otherwise (or
    /// when the stem is not a valid slug).
    name: Option<String>,
    ast: Option<AstModule>,
    /// Normalised library load paths this file imports.
    deps: Vec<String>,
    /// Load paths that break the `events/`-or-`lib/` restriction. A file with any
    /// is not evaluated; its finding is the actionable one.
    illegal_deps: Vec<String>,
}

/// Accumulates event definitions across `events/` modules, tracking where each
/// type was first defined so a collision names the other file.
#[derive(Default)]
struct EventCollector {
    by_type: HashMap<String, EventDef>,
    defined_in: HashMap<String, String>,
}

fn discover_and_parse(root: &Path, findings: &mut Vec<Finding>) -> Vec<ParsedFile> {
    let mut files = Vec::new();
    for subdir in ["events", "lib", "commands", "projectors", "effects"] {
        let dir = root.join(subdir);
        if !dir.is_dir() {
            continue;
        }
        for path in star_files(&dir) {
            let rel = rel_to_string(root, &path);
            let role = match subdir {
                "events" => Role::Events,
                "lib" => Role::Lib,
                "commands" => Role::Command {
                    internal: rel.starts_with("commands/internal/"),
                },
                "projectors" => Role::Projector,
                "effects" => Role::Effect,
                _ => unreachable!("subdir list is fixed"),
            };
            files.push(parse_one(&path, rel, role, findings));
        }
    }
    files
}

fn parse_one(path: &Path, rel: String, role: Role, findings: &mut Vec<Finding>) -> ParsedFile {
    let load_path = match role {
        Role::Events | Role::Lib => Some(rel.clone()),
        _ => None,
    };
    let name = match role {
        Role::Command { .. } | Role::Projector | Role::Effect => {
            match module_name_from_path(&rel) {
                Ok(name) => Some(name),
                Err(err) => {
                    findings.push(Finding::error(&rel, format!("{err:#}")));
                    None
                }
            }
        }
        Role::Events | Role::Lib => None,
    };

    let mut file = ParsedFile {
        role,
        rel_path: rel,
        load_path,
        name,
        ast: None,
        deps: Vec::new(),
        illegal_deps: Vec::new(),
    };

    let src = match fs::read_to_string(path) {
        Ok(src) => src,
        Err(err) => {
            findings.push(Finding::error(
                &file.rel_path,
                format!("reading file: {err}"),
            ));
            return file;
        }
    };
    let ast = match parse_module(&file.rel_path, src) {
        Ok(ast) => ast,
        Err(err) => {
            findings.push(Finding::error(&file.rel_path, format!("{err}")));
            return file;
        }
    };

    for load in ast.loads() {
        match normalize_load_path(load.module_id) {
            Ok(norm) if is_library_path(&norm) => file.deps.push(norm),
            Ok(norm) => {
                findings.push(Finding::error(
                    &file.rel_path,
                    format!(
                        "load(\"{}\") is not allowed; a {} may only load from events/ or lib/",
                        load.module_id,
                        role.label()
                    ),
                ));
                file.illegal_deps.push(norm);
            }
            Err(msg) => {
                findings.push(Finding::error(&file.rel_path, msg));
                file.illegal_deps.push(load.module_id.to_owned());
            }
        }
    }
    file.ast = Some(ast);
    file
}

/// Evaluate `events/` and `lib/` modules in dependency order, filling `cache`
/// (the `load()` targets) and `collector` (the event registry).
fn evaluate_libraries(
    parsed: &mut [ParsedFile],
    globals: &Globals,
    cache: &mut HashMap<String, FrozenModule>,
    collector: &mut EventCollector,
    findings: &mut Vec<Finding>,
) {
    let present: HashSet<String> = parsed
        .iter()
        .filter(|file| matches!(file.role, Role::Events | Role::Lib))
        .filter_map(|file| file.load_path.clone())
        .collect();

    let mut pending: Vec<usize> = parsed
        .iter()
        .enumerate()
        .filter(|(_, file)| {
            matches!(file.role, Role::Events | Role::Lib)
                && file.ast.is_some()
                && file.illegal_deps.is_empty()
        })
        .map(|(idx, _)| idx)
        .collect();

    // Evaluate any module whose in-project library deps are already cached,
    // repeating until no more become ready. What remains is cyclic or depends on
    // a module that failed to evaluate.
    loop {
        let ready = pending.iter().position(|&idx| {
            parsed[idx]
                .deps
                .iter()
                .filter(|dep| present.contains(*dep))
                .all(|dep| cache.contains_key(dep))
        });
        let Some(pos) = ready else { break };
        let idx = pending.remove(pos);
        evaluate_one_library(&mut parsed[idx], globals, cache, collector, findings);
    }

    for &idx in &pending {
        let file = &parsed[idx];
        let unresolved: Vec<&str> = file
            .deps
            .iter()
            .filter(|dep| !cache.contains_key(*dep))
            .map(String::as_str)
            .collect();
        findings.push(Finding::error(
            &file.rel_path,
            format!("could not evaluate: unresolved or cyclic load() of {unresolved:?}"),
        ));
    }
}

fn evaluate_one_library(
    file: &mut ParsedFile,
    globals: &Globals,
    cache: &mut HashMap<String, FrozenModule>,
    collector: &mut EventCollector,
    findings: &mut Vec<Finding>,
) {
    let ast = file.ast.take().expect("pending files carry an ast");
    let load_path = file
        .load_path
        .clone()
        .expect("library files have a load path");
    let loader = LibraryLoader { cache };
    match eval_frozen(ast, globals, Some(&loader)) {
        Ok(frozen) => {
            if matches!(file.role, Role::Events) {
                register_events(&file.rel_path, &frozen, collector, findings);
            }
            cache.insert(load_path, frozen);
        }
        Err(err) => findings.push(Finding::error(&file.rel_path, format!("{err}"))),
    }
}

/// Pull every `EventDef` bound at module scope into the registry, flagging a
/// type defined in two places.
fn register_events(
    rel: &str,
    frozen: &FrozenModule,
    collector: &mut EventCollector,
    findings: &mut Vec<Finding>,
) {
    let bindings: Vec<String> = frozen
        .names()
        .filter_map(|name| name.to_value().unpack_str().map(str::to_owned))
        .collect();
    for binding in bindings {
        let Ok(Some(owned)) = frozen.get_option(&binding) else {
            continue;
        };
        let Some(def) = owned.value().downcast_ref::<EventDef>() else {
            continue;
        };
        if let Some(other) = collector.defined_in.get(&def.event_type) {
            findings.push(Finding::error(
                rel,
                format!(
                    "event type `{}` is already defined in {}",
                    def.event_type, other
                ),
            ));
            continue;
        }
        collector
            .defined_in
            .insert(def.event_type.clone(), rel.to_owned());
        collector
            .by_type
            .insert(def.event_type.clone(), def.clone());
    }
}

/// Evaluate command, projector and effect modules against the library cache.
fn evaluate_units(
    parsed: &mut [ParsedFile],
    command_globals: &Globals,
    projector_globals: &Globals,
    effect_globals: &Globals,
    cache: &HashMap<String, FrozenModule>,
    findings: &mut Vec<Finding>,
) -> (Vec<CommandUnit>, Vec<ProjectorUnit>, Vec<EffectUnit>) {
    let loader = LibraryLoader { cache };
    let mut commands = Vec::new();
    let mut projectors = Vec::new();
    let mut effects = Vec::new();

    for file in parsed.iter_mut() {
        let Some(kind) = file.role.module_kind() else {
            continue;
        };
        if !file.illegal_deps.is_empty() {
            continue;
        }
        let (Some(name), Some(ast)) = (file.name.clone(), file.ast.take()) else {
            continue;
        };
        let rel = file.rel_path.clone();

        // Commands get `now()`, projectors get `get()`, effects get the impure
        // builtins. Selecting per kind is what keeps purity structural.
        let globals = match kind {
            ModuleKind::Command => command_globals,
            ModuleKind::Effect => effect_globals,
            ModuleKind::Projector => projector_globals,
        };
        let frozen = match eval_frozen(ast, globals, Some(&loader)) {
            Ok(frozen) => frozen,
            Err(err) => {
                findings.push(Finding::error(&rel, format!("{err}")));
                continue;
            }
        };
        let def = match module_def_from_frozen(kind, name, &rel, &frozen) {
            Ok(def) => def,
            Err(err) => {
                findings.push(Finding::error(&rel, format!("{err:#}")));
                continue;
            }
        };
        let loaded = LoadedModule {
            def,
            module: frozen,
        };
        match file.role {
            Role::Command { internal } => commands.push(CommandUnit {
                loaded,
                rel_path: rel,
                internal,
            }),
            Role::Projector => projectors.push(ProjectorUnit {
                loaded,
                rel_path: rel,
            }),
            Role::Effect => effects.push(EffectUnit {
                loaded,
                rel_path: rel,
            }),
            Role::Events | Role::Lib => unreachable!("filtered by module_kind"),
        }
    }
    (commands, projectors, effects)
}

fn check_name_collisions(
    commands: &[CommandUnit],
    projectors: &[ProjectorUnit],
    effects: &[EffectUnit],
    findings: &mut Vec<Finding>,
) {
    report_duplicates(
        commands
            .iter()
            .map(|unit| (unit.loaded.def.name(), unit.rel_path.as_str())),
        "command",
        findings,
    );
    report_duplicates(
        projectors
            .iter()
            .map(|unit| (unit.loaded.def.name(), unit.rel_path.as_str())),
        "projector",
        findings,
    );
    report_duplicates(
        effects
            .iter()
            .map(|unit| (unit.loaded.def.name(), unit.rel_path.as_str())),
        "effect",
        findings,
    );
}

fn report_duplicates<'a>(
    items: impl Iterator<Item = (&'a str, &'a str)>,
    kind: &str,
    findings: &mut Vec<Finding>,
) {
    let mut seen: HashMap<&str, &str> = HashMap::new();
    for (name, rel) in items {
        match seen.get(name) {
            Some(first) => findings.push(Finding::error(
                rel,
                format!("{kind} name `{name}` is already used by {first}"),
            )),
            None => {
                seen.insert(name, rel);
            }
        }
    }
}

/// Resolves `load()` paths against the evaluated-module cache, enforcing that
/// imports come only from `events/` or `lib/`.
struct LibraryLoader<'a> {
    cache: &'a HashMap<String, FrozenModule>,
}

impl FileLoader for LibraryLoader<'_> {
    fn load(&self, path: &str) -> starlark::Result<FrozenModule> {
        let norm = normalize_load_path(path).map_err(|err| anyhow::anyhow!("{err}"))?;
        if !is_library_path(&norm) {
            return Err(anyhow::anyhow!(
                "load(\"{path}\") is not allowed; modules may only load from events/ or lib/"
            )
            .into());
        }
        match self.cache.get(&norm) {
            Some(module) => Ok(module.clone()),
            None => Err(anyhow::anyhow!(
                "load(\"{path}\") could not be resolved to a known module"
            )
            .into()),
        }
    }
}

fn is_library_path(path: &str) -> bool {
    path.starts_with("events/") || path.starts_with("lib/")
}

/// Normalise a `load()` path to a project-relative, forward-slash form matching
/// the cache keys. Rejects absolute paths and `..` so imports cannot escape the
/// project.
fn normalize_load_path(raw: &str) -> Result<String, String> {
    let trimmed = raw.strip_prefix("./").unwrap_or(raw);
    if trimmed.is_empty() {
        return Err("load path is empty".to_owned());
    }
    if trimmed.starts_with('/') {
        return Err(format!(
            "load path `{raw}` must be relative to the project root"
        ));
    }
    if trimmed.split('/').any(|part| part == "..") {
        return Err(format!("load path `{raw}` must not contain `..`"));
    }
    Ok(trimmed.to_owned())
}

fn star_files(dir: &Path) -> Vec<PathBuf> {
    WalkDir::new(dir)
        .sort_by_file_name()
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("star"))
        .collect()
}

fn rel_to_string(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod behaviour_tests {
    use std::path::Path;

    use starlark::environment::Module;

    use super::LoadedProject;
    use crate::starlark_builtins::{
        HandleOutcome, ModuleDef, alloc_input, call_handler, parse_handle_result, thaw,
    };

    fn register_user(project: &LoadedProject) -> &super::CommandUnit {
        project
            .commands
            .iter()
            .find(|unit| unit.loaded.def.name() == "register-user")
            .expect("register-user command")
    }

    /// Run `register-user`'s handle against a synthetic input and prior state,
    /// exercising the whole language chain: load() resolution, the event-def
    /// constructor, payload validation, tag derivation and emit.
    fn run_handle(name: &str, state_taken: bool) -> HandleOutcome {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/users");
        let project = LoadedProject::load(&root);
        assert!(!project.has_errors(), "{:?}", project.findings);
        let command = register_user(&project);
        let ModuleDef::Command { input: schema, .. } = &command.loaded.def else {
            panic!("expected a command");
        };
        let frozen = &command.loaded.module;
        Module::with_temp_heap(|module| {
            let payload = serde_json::json!({
                "user_id": "11111111-1111-1111-1111-111111111111",
                "email": "alice@example.com",
                "name": name,
            });
            let input = alloc_input(&module, schema, &payload).unwrap();
            let state = module
                .heap()
                .alloc(serde_json::json!({"taken": state_taken}));
            let handle = frozen.get_option("handle").unwrap().unwrap();
            let result =
                call_handler(&module, thaw(&handle, &module), &[input, state], 10_000_000).unwrap();
            parse_handle_result(result).unwrap()
        })
    }

    #[test]
    fn emits_a_validated_tagged_event() {
        let HandleOutcome::Emit(events) = run_handle("Alice", false) else {
            panic!("expected emit");
        };
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.event_type, "user.registered");
        assert_eq!(event.data["email"], serde_json::json!("alice@example.com"));
        assert!(
            event
                .tags
                .contains(&("email".to_owned(), Some("alice@example.com".to_owned())))
        );
        assert!(event.tags.contains(&(
            "user_id".to_owned(),
            Some("11111111-1111-1111-1111-111111111111".to_owned())
        )));
    }

    #[test]
    fn rejects_a_blank_name_via_the_lib_helper() {
        let HandleOutcome::Reject(rejection) = run_handle("   ", false) else {
            panic!("expected reject");
        };
        assert_eq!(rejection.code, "invalid_name");
    }

    #[test]
    fn rejects_a_taken_email() {
        let HandleOutcome::Reject(rejection) = run_handle("Alice", true) else {
            panic!("expected reject");
        };
        assert_eq!(rejection.code, "email_taken");
    }
}
