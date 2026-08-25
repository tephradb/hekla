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

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use starlark::codemap::ResolvedSpan;
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
    /// Where in `location` the problem is, when it came from something that
    /// carries a span (a parse error, a `load()` statement, an evaluation
    /// failure). Findings about a file as a whole leave it `None`. Line and
    /// column are 0-based.
    pub span: Option<ResolvedSpan>,
}

impl Finding {
    pub fn error(location: impl Into<String>, message: impl Into<String>) -> Finding {
        Finding {
            severity: Severity::Error,
            location: location.into(),
            message: message.into(),
            span: None,
        }
    }

    pub fn warning(location: impl Into<String>, message: impl Into<String>) -> Finding {
        Finding {
            severity: Severity::Warning,
            location: location.into(),
            message: message.into(),
            span: None,
        }
    }

    /// Anchor this finding to a source span.
    pub fn with_span(mut self, span: ResolvedSpan) -> Finding {
        self.span = Some(span);
        self
    }

    /// An error finding from a starlark error: parse failures and evaluation
    /// failures carry a span, so take the bare message and report the position
    /// separately. Without a span the rendered form (traceback and source
    /// excerpt) is the only positional information there is, so keep it.
    pub fn from_starlark_error(location: impl Into<String>, err: &starlark::Error) -> Finding {
        match err.span() {
            Some(span) => Finding::error(location, err.without_diagnostic().to_string())
                .with_span(span.resolve_span()),
            None => Finding::error(location, format!("{err}")),
        }
    }
}

/// The event definitions declared across `events/`, plus the evaluated library
/// modules kept as `load()` targets (the evaluated-module cache).
pub struct EventRegistry {
    /// Event type string to its definition.
    pub by_type: HashMap<String, EventDef>,
    /// Evaluated `events/` and `lib/` modules, keyed by their load path.
    library: HashMap<String, FrozenModule>,
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
    /// Every `events/` and `lib/` file found, sorted. These are exactly the legal
    /// `load()` targets, listed whether or not each one evaluated, so a caller can
    /// offer them even while the project has errors.
    pub library_paths: Vec<String>,
    /// Problems found while loading. `kiln check` adds semantic findings on top.
    pub findings: Vec<Finding>,
}

/// The shared body of [`LoadedProject::load`] and [`LoadedProject::load_libraries`].
/// `units` decides whether commands, projectors and effects are parsed and
/// evaluated at all; the library half is identical either way.
fn load_inner(root: &Path, units: bool) -> LoadedProject {
    let mut findings = Vec::new();

    let config = match Config::load(root) {
        Ok(config) => config,
        Err(err) => {
            findings.push(Finding::error(config::FILE_NAME, format!("{err:#}")));
            Config::default()
        }
    };

    let base = starlark_builtins::globals();
    let mut parsed = discover_and_parse(root, units, &mut findings);

    let mut library_paths: Vec<String> = parsed
        .iter()
        .filter_map(|file| file.load_path.clone())
        .collect();
    library_paths.sort();

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

    let (commands, projectors, effects) = if units {
        let command = starlark_builtins::command_globals();
        let projector = starlark_builtins::projector_globals();
        let effect = starlark_builtins::effect_globals();
        let units = evaluate_units(
            &mut parsed,
            &command,
            &projector,
            &effect,
            &cache,
            &collector.by_type,
            &mut findings,
        );
        check_name_collisions(&units.0, &units.1, &units.2, &mut findings);
        units
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };

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
        library_paths,
        findings,
    }
}

impl LoadedProject {
    /// Load a project from `root`, collecting findings rather than bailing.
    pub fn load(root: &Path) -> LoadedProject {
        load_inner(root, true)
    }

    /// Load only the shared half of a project: the config, the event registry and
    /// the evaluated `events/`/`lib/` modules, with no commands, projectors or
    /// effects. Enough to evaluate one module against, and much cheaper than a
    /// full load, so a caller that re-reads a project repeatedly (the language
    /// server) can do so without evaluating every unit each time.
    pub fn load_libraries(root: &Path) -> LoadedProject {
        load_inner(root, false)
    }

    pub fn has_errors(&self) -> bool {
        self.findings
            .iter()
            .any(|finding| finding.severity == Severity::Error)
    }

    /// Evaluate an extra module against this project's library cache, so it can
    /// `load()` from `events/` and `lib/` exactly as a deployed module would. The
    /// module is frozen against `globals`, which must be the set its role gets.
    ///
    /// `query_mode` must match the role: a command, projector or effect names
    /// query clauses at module top level, a test file constructs events instead.
    pub fn eval_against_libraries(
        &self,
        filename: &str,
        src: String,
        globals: &Globals,
        query_mode: bool,
    ) -> anyhow::Result<FrozenModule> {
        let ast = parse_module(filename, src).map_err(|err| anyhow::anyhow!("{err}"))?;
        self.eval_ast_against_libraries(ast, globals, query_mode)
            .map_err(|err| anyhow::anyhow!("{err}"))
    }

    /// As [`Self::eval_against_libraries`], but over an already-parsed module and
    /// keeping the starlark error, which carries a span. A caller that has parsed
    /// the source for its own reasons should not pay to parse it twice.
    pub fn eval_ast_against_libraries(
        &self,
        ast: AstModule,
        globals: &Globals,
        query_mode: bool,
    ) -> starlark::Result<FrozenModule> {
        let loader = LibraryLoader {
            cache: &self.events.library,
        };
        eval_frozen(ast, globals, Some(&loader), query_mode)
    }
}

/// The directories a kiln project is made of. `tests/` is last because the loader
/// does not walk it: `kiln test` does, and the language server needs to know the
/// convention either way.
pub const MODULE_DIRS: [&str; 6] = [
    "events",
    "lib",
    "commands",
    "projectors",
    "effects",
    "tests",
];

/// Which directory convention a file falls under. `Command` also records whether
/// the file sits in `commands/internal/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Events,
    Lib,
    Command {
        internal: bool,
    },
    Projector,
    Effect,
    /// A `kiln test` scenario file. Never loaded as part of a deployment, so it
    /// has no [`ModuleKind`], but it is part of the directory convention.
    Test,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Role::Events => "event module",
            Role::Lib => "library module",
            Role::Command { .. } => "command",
            Role::Projector => "projector",
            Role::Effect => "effect",
            Role::Test => "test module",
        }
    }

    /// The module kind for a command, projector or effect; `None` for library and
    /// test files, which have no `ModuleDef`.
    pub fn module_kind(self) -> Option<ModuleKind> {
        match self {
            Role::Command { .. } => Some(ModuleKind::Command),
            Role::Projector => Some(ModuleKind::Projector),
            Role::Effect => Some(ModuleKind::Effect),
            Role::Events | Role::Lib | Role::Test => None,
        }
    }
}

/// The role a project-relative path falls under, or `None` when it is not a kiln
/// module. The single source of truth for the directory convention: the loader,
/// `kiln test` and the language server all route through it, so the three cannot
/// drift.
///
/// `rel` is project-relative with forward slashes, as [`rel_to_string`] produces.
pub fn role_for(rel: &str) -> Option<Role> {
    let dir = rel.split('/').next()?;
    if !rel.ends_with(".star") || rel.len() <= dir.len() + 1 {
        return None;
    }
    match dir {
        "events" => Some(Role::Events),
        "lib" => Some(Role::Lib),
        // Nesting is free, so `commands/billing/refund.star` is public; only the
        // literal `commands/internal/` prefix marks a command internal.
        "commands" => Some(Role::Command {
            internal: rel.starts_with("commands/internal/"),
        }),
        "projectors" => Some(Role::Projector),
        "effects" => Some(Role::Effect),
        "tests" => Some(Role::Test),
        _ => None,
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
    /// The source hash, recorded as the module's deployed identity and, for
    /// effects, as the script hash on each invocation.
    source_hash: String,
    /// Normalised library load paths this file imports.
    deps: Vec<String>,
    /// Local names bound by this file's `load()` statements. A frozen module
    /// re-exports its loaded symbols, so these mark bindings that are imports, not
    /// definitions, and must not be registered as events by this module.
    loaded_names: HashSet<String>,
    /// Set when a load path breaks the `events/`-or-`lib/` restriction. Such a file
    /// is not evaluated; the finding recorded at parse time is the actionable one.
    has_illegal_deps: bool,
}

/// Accumulates event definitions across `events/` modules, tracking where each
/// type was first defined so a collision names the other file.
#[derive(Default)]
struct EventCollector {
    by_type: HashMap<String, EventDef>,
    defined_in: HashMap<String, String>,
}

fn discover_and_parse(root: &Path, units: bool, findings: &mut Vec<Finding>) -> Vec<ParsedFile> {
    let mut files = Vec::new();
    // `tests/` is deliberately skipped: it is part of the convention but not part
    // of a deployment, so `kiln test` walks it separately. Unit directories are
    // skipped too when the caller only wants the library half.
    let wanted: &[&str] = if units {
        &["events", "lib", "commands", "projectors", "effects"]
    } else {
        &["events", "lib"]
    };
    for subdir in MODULE_DIRS.iter().filter(|dir| wanted.contains(dir)) {
        let dir = root.join(subdir);
        if !dir.is_dir() {
            continue;
        }
        for path in star_files(&dir, findings) {
            let rel = rel_to_string(root, &path);
            let Some(role) = role_for(&rel) else {
                continue;
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
        Role::Events | Role::Lib | Role::Test => None,
    };

    let mut file = ParsedFile {
        role,
        rel_path: rel,
        load_path,
        name,
        ast: None,
        source_hash: String::new(),
        deps: Vec::new(),
        loaded_names: HashSet::new(),
        has_illegal_deps: false,
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
    file.source_hash = crate::hash::sha256_hex(src.as_bytes());
    let ast = match parse_module(&file.rel_path, src) {
        Ok(ast) => ast,
        Err(err) => {
            findings.push(Finding::from_starlark_error(&file.rel_path, &err));
            return file;
        }
    };

    for load in ast.loads() {
        file.loaded_names
            .extend(load.symbols.iter().map(|(local, _)| local.to_string()));
        let span = load.span.resolve_span();
        match normalize_load_path(load.module_id) {
            Ok(norm) if is_library_path(&norm) => file.deps.push(norm),
            Ok(_) => {
                findings.push(
                    Finding::error(
                        &file.rel_path,
                        format!(
                            "load(\"{}\") is not allowed; a {} may only load from events/ or lib/",
                            load.module_id,
                            role.label()
                        ),
                    )
                    .with_span(span),
                );
                file.has_illegal_deps = true;
            }
            Err(msg) => {
                findings.push(Finding::error(&file.rel_path, msg).with_span(span));
                file.has_illegal_deps = true;
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
                && !file.has_illegal_deps
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
    match eval_frozen(ast, globals, Some(&loader), false) {
        Ok(frozen) => {
            if matches!(file.role, Role::Events) {
                register_events(
                    &file.rel_path,
                    &frozen,
                    &file.loaded_names,
                    collector,
                    findings,
                );
            } else {
                reject_event_definition(
                    &file.rel_path,
                    file.role,
                    &frozen,
                    &file.loaded_names,
                    &collector.by_type,
                    findings,
                );
            }
            cache.insert(load_path, frozen);
        }
        Err(err) => findings.push(Finding::from_starlark_error(&file.rel_path, &err)),
    }
}

/// Every `EventDef` bound at this module's scope. A frozen module re-exports the
/// symbols it `load()`s, so a binding that names an import is skipped: a shared
/// event referenced from a second module is one definition, not a second one.
fn module_event_defs(frozen: &FrozenModule, loaded_names: &HashSet<String>) -> Vec<EventDef> {
    let bindings: Vec<&str> = frozen
        .names()
        .filter_map(|name| name.to_value().unpack_str())
        .collect();
    let mut defs = Vec::new();
    for binding in bindings {
        if loaded_names.contains(binding) {
            continue;
        }
        let Ok(Some(owned)) = frozen.get_option(binding) else {
            continue;
        };
        if let Some(def) = owned.value().downcast_ref::<EventDef>() {
            defs.push(def.clone());
        }
    }
    defs
}

/// Pull every `EventDef` defined at module scope into the registry, flagging a
/// type defined in two places.
fn register_events(
    rel: &str,
    frozen: &FrozenModule,
    loaded_names: &HashSet<String>,
    collector: &mut EventCollector,
    findings: &mut Vec<Finding>,
) {
    for def in module_event_defs(frozen, loaded_names) {
        // The same definition re-exported under a second name is one event, not a
        // collision; only a genuinely different definition of the type is.
        if collector
            .by_type
            .get(&def.event_type)
            .is_some_and(|known| known.id == def.id)
        {
            continue;
        }
        if let Some(other) = collector.defined_in.get(&def.event_type) {
            findings.push(Finding::error(
                rel,
                format!(
                    "event type `{}` is already defined in {other}",
                    def.event_type
                ),
            ));
            continue;
        }
        collector
            .defined_in
            .insert(def.event_type.clone(), rel.to_owned());
        collector.by_type.insert(def.event_type.clone(), def);
    }
}

/// Reject an event defined outside `events/`. Only `events/` modules feed the
/// registry, so such a definition is invisible to the runtime: dispatch falls back
/// to its no-schema path and writes a `subject` field to the immutable log as
/// plaintext, in the payload and in its tag, where it can never be erased.
///
/// Re-binding a loaded definition under a second name (`Alias = thing_done`) is not
/// a definition, and [`module_event_defs`] cannot tell the two apart: it skips only
/// the exact `load()` local name. Comparing [`EventDef::id`] can, so an alias passes
/// while a fresh `event(...)` reusing a declared type name is still caught here
/// rather than at the first append.
fn reject_event_definition(
    rel: &str,
    role: Role,
    frozen: &FrozenModule,
    loaded_names: &HashSet<String>,
    registered: &HashMap<String, EventDef>,
    findings: &mut Vec<Finding>,
) {
    for def in module_event_defs(frozen, loaded_names) {
        let message = match registered.get(&def.event_type) {
            Some(known) if known.id == def.id => continue,
            Some(_) => format!(
                "event type `{}` is redeclared in a {}; load() the events/ definition instead, since only that one is registered",
                def.event_type,
                role.label()
            ),
            None => format!(
                "event type `{}` is defined in a {}; move the definition to events/ and load() it here, since only events/ definitions are registered",
                def.event_type,
                role.label()
            ),
        };
        findings.push(Finding::error(rel, message));
    }
}

/// Evaluate command, projector and effect modules against the library cache.
fn evaluate_units(
    parsed: &mut [ParsedFile],
    command_globals: &Globals,
    projector_globals: &Globals,
    effect_globals: &Globals,
    cache: &HashMap<String, FrozenModule>,
    registered: &HashMap<String, EventDef>,
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
        if file.has_illegal_deps {
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
        // Every kind names query clauses at module top level: a projector's or
        // effect's `handle` keys, a command's `fold` keys. Events are constructed
        // inside `handle`, which runs in its own evaluator, so this cannot reach it.
        let query_mode = true;
        let frozen = match eval_frozen(ast, globals, Some(&loader), query_mode) {
            Ok(frozen) => frozen,
            Err(err) => {
                findings.push(Finding::from_starlark_error(&rel, &err));
                continue;
            }
        };
        reject_event_definition(
            &rel,
            file.role,
            &frozen,
            &file.loaded_names,
            registered,
            findings,
        );
        let def = match module_def_from_frozen(kind, name, &frozen) {
            Ok(def) => def,
            Err(err) => {
                findings.push(Finding::error(&rel, format!("{err:#}")));
                continue;
            }
        };
        let loaded = LoadedModule {
            def,
            module: frozen,
            source_hash: file.source_hash.clone(),
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
            Role::Events | Role::Lib | Role::Test => unreachable!("filtered by module_kind"),
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
        match seen.entry(name) {
            Entry::Occupied(first) => findings.push(Finding::error(
                rel,
                format!("{kind} name `{name}` is already used by {}", first.get()),
            )),
            Entry::Vacant(slot) => {
                slot.insert(rel);
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

/// Whether a normalised load path names a shared module. Every role may only
/// load from `events/` or `lib/`, including those two themselves.
pub fn is_library_path(path: &str) -> bool {
    path.starts_with("events/") || path.starts_with("lib/")
}

/// Normalise a `load()` path to a project-relative, forward-slash form matching
/// the cache keys. Rejects absolute paths and `..` so imports cannot escape the
/// project.
pub fn normalize_load_path(raw: &str) -> Result<String, String> {
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

/// The `.star` files under `dir`, in a stable order. A subtree that cannot be
/// walked is reported rather than dropped: a silently missing command would let
/// `kiln check` pass on a project the runtime cannot fully load.
pub(crate) fn star_files(dir: &Path, findings: &mut Vec<Finding>) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in WalkDir::new(dir).sort_by_file_name() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                findings.push(Finding::error(
                    dir.display().to_string(),
                    format!("walking the project tree: {err}"),
                ));
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.into_path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("star") {
            files.push(path);
        }
    }
    files
}

pub(crate) fn rel_to_string(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod role_tests {
    use super::{Role, role_for};

    #[test]
    fn each_module_directory_maps_to_its_role() {
        assert_eq!(role_for("events/user.star"), Some(Role::Events));
        assert_eq!(role_for("lib/validation.star"), Some(Role::Lib));
        assert_eq!(role_for("projectors/users.star"), Some(Role::Projector));
        assert_eq!(role_for("effects/send-welcome.star"), Some(Role::Effect));
        assert_eq!(role_for("tests/register-user.star"), Some(Role::Test));
    }

    #[test]
    fn only_the_internal_prefix_makes_a_command_internal() {
        assert_eq!(
            role_for("commands/register-user.star"),
            Some(Role::Command { internal: false })
        );
        // Nesting is free; the internal marker is the literal prefix, not depth.
        assert_eq!(
            role_for("commands/billing/refund.star"),
            Some(Role::Command { internal: false })
        );
        assert_eq!(
            role_for("commands/internal/record-welcome.star"),
            Some(Role::Command { internal: true })
        );
    }

    #[test]
    fn a_path_outside_the_convention_has_no_role() {
        assert_eq!(role_for("kiln.toml"), None);
        assert_eq!(role_for("src/main.star"), None);
        assert_eq!(role_for("commands/notes.md"), None);
        // A bare directory is not a module, and neither is a directory that merely
        // starts with a module directory's name.
        assert_eq!(role_for("commands"), None);
        assert_eq!(role_for("commands/"), None);
        assert_eq!(role_for("commandsx/a.star"), None);
    }
}

#[cfg(test)]
mod library_load_tests {
    use std::path::Path;

    use super::LoadedProject;

    fn example(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join(name)
    }

    /// The cheap load has to agree with the full one about the shared half, or a
    /// caller that uses it (the language server) would resolve `load()`s and event
    /// clauses differently from `kiln check`.
    #[test]
    fn loading_libraries_only_agrees_with_a_full_load() {
        for name in ["users", "orders"] {
            let root = example(name);
            let full = LoadedProject::load(&root);
            let libraries = LoadedProject::load_libraries(&root);

            let mut full_events: Vec<&str> =
                full.events.by_type.keys().map(String::as_str).collect();
            let mut library_events: Vec<&str> = libraries
                .events
                .by_type
                .keys()
                .map(String::as_str)
                .collect();
            full_events.sort();
            library_events.sort();
            assert_eq!(full_events, library_events, "{name}: event registry");

            assert_eq!(
                full.library_paths, libraries.library_paths,
                "{name}: library paths"
            );
            assert!(
                !libraries.library_paths.is_empty(),
                "{name}: expected some library modules"
            );
            assert!(!libraries.has_errors(), "{name}: {:?}", libraries.findings);

            // The units are the whole difference.
            assert!(libraries.commands.is_empty());
            assert!(libraries.projectors.is_empty());
            assert!(libraries.effects.is_empty());
            assert!(!full.commands.is_empty(), "{name}: expected commands");
        }
    }
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
            parse_handle_result(result, &project.events.by_type).unwrap()
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
