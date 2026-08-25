//! Finding the hekla project a document belongs to, and keeping its shared half
//! loaded.
//!
//! Two problems, with the same answer. A document arrives as a bare path, and
//! nothing in the protocol says which project it is part of: `hekla.toml` is
//! optional, and one workspace can hold several projects (`examples/` here holds
//! two). And diagnosing a buffer means evaluating it against the project's
//! `events/` and `lib/` modules, which would be far too slow to redo per
//! keystroke.
//!
//! So a project's shared half is loaded once and cached, keyed by root, and
//! reloaded only when the files that make it up change on disk. The cache holds
//! **disk state only**: an editor buffer's text never enters it. That is what
//! keeps invalidation tractable, and it is not a shortcut — the protocol gives no
//! signal on which an overlay could be dropped, because a document being closed
//! never reaches this code.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Instant, SystemTime};

use crate::config;
use crate::loader::{LoadedProject, Role, rel_to_string, role_for};

/// How often a project's files may be stat-ed to see whether the snapshot is
/// stale. There is no file watching to be had: the server registers no
/// `didChangeWatchedFiles` capability and is never told about saves, so polling
/// is what lets a `git checkout`, a second editor or `hekla fmt` correct itself
/// without a restart.
const STAT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// How long to remember that a directory is not inside a hekla project. Short,
/// because creating the project is exactly the thing that makes it wrong.
const MISS_TTL: std::time::Duration = std::time::Duration::from_secs(5);

/// How far up the tree to look for a project root before giving up.
const MAX_ASCENT: usize = 64;

/// A document placed inside a hekla project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Located {
    pub(crate) root: PathBuf,
    /// Project-relative, forward slashes: the form the loader's findings, its
    /// `load()` cache keys and its module names all use.
    pub(crate) rel: String,
    pub(crate) role: Role,
}

/// A project's shared half, as it was on disk when this was taken.
pub(crate) struct Snapshot {
    pub(crate) project: LoadedProject,
}

/// A cached snapshot, with what it was built from and when that was last
/// confirmed. Both live outside the `Arc` so confirming freshness does not mean
/// rebuilding, or reaching for interior mutability to avoid it.
struct Cached {
    snapshot: Arc<Snapshot>,
    fingerprint: String,
    checked: Instant,
}

/// The project cache.
pub(crate) struct Projects {
    /// Directory to the project root containing it, if any.
    roots: RwLock<HashMap<PathBuf, (Option<PathBuf>, Instant)>>,
    by_root: RwLock<HashMap<PathBuf, Cached>>,
}

impl Projects {
    pub(crate) fn new() -> Projects {
        Projects {
            roots: RwLock::new(HashMap::new()),
            by_root: RwLock::new(HashMap::new()),
        }
    }

    /// Place a document: which project it is in, where in it, and as what.
    pub(crate) fn locate(&self, path: &Path) -> Option<Located> {
        let dir = path.parent()?;
        let root = self.root_for(dir)?;
        let rel = rel_to_string(&root, path);
        let role = role_for(&rel)?;
        Some(Located { root, rel, role })
    }

    /// The project root for a directory, cached. A hit is cached forever (a root
    /// does not stop being one); a miss expires, since the answer changes the
    /// moment someone creates `commands/`.
    fn root_for(&self, dir: &Path) -> Option<PathBuf> {
        if let Some((cached, at)) = self.roots.read().unwrap().get(dir)
            && (cached.is_some() || at.elapsed() < MISS_TTL)
        {
            return cached.clone();
        }
        let found = find_root(dir);
        self.roots
            .write()
            .unwrap()
            .insert(dir.to_path_buf(), (found.clone(), Instant::now()));
        found
    }

    /// The shared half of the project at `root`, reloading it only when the files
    /// it is built from have changed and the last check was long enough ago.
    pub(crate) fn snapshot(&self, root: &Path) -> Arc<Snapshot> {
        if let Some(cached) = self.by_root.read().unwrap().get(root)
            && cached.checked.elapsed() < STAT_INTERVAL
        {
            return Arc::clone(&cached.snapshot);
        }

        let fingerprint = fingerprint(root);
        let mut by_root = self.by_root.write().unwrap();
        if let Some(cached) = by_root.get_mut(root)
            && cached.fingerprint == fingerprint
        {
            // Nothing the snapshot is built from has changed, so keep it and do
            // not stat again for another interval.
            cached.checked = Instant::now();
            return Arc::clone(&cached.snapshot);
        }

        let snapshot = Arc::new(Snapshot {
            project: LoadedProject::load_libraries(root),
        });
        by_root.insert(
            root.to_path_buf(),
            Cached {
                snapshot: Arc::clone(&snapshot),
                fingerprint,
                checked: Instant::now(),
            },
        );
        snapshot
    }
}

/// Walk up from `dir` looking for the project root, deepest first so a project
/// nested inside another wins.
fn find_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .take(MAX_ASCENT)
        .find(|candidate| is_project_root(candidate))
        .map(Path::to_path_buf)
}

/// Whether a directory looks like a hekla project root.
///
/// `hekla.toml` is optional, so the directory convention has to stand on its own.
/// `tests/` and `lib/` are deliberately not enough: both are ordinary names that
/// any repository might have (this one has both), and mistaking a repository for
/// a hekla project would attach hekla's builtins to unrelated Starlark.
fn is_project_root(dir: &Path) -> bool {
    dir.join(config::FILE_NAME).is_file()
        || ["events", "commands", "projectors", "effects"]
            .iter()
            .any(|name| dir.join(name).is_dir())
}

/// A digest of everything a snapshot is built from: the config and every
/// `events/` and `lib/` file, by path, size and modification time.
///
/// Commands, projectors, effects and tests are deliberately absent. A snapshot
/// does not contain them, so editing one must not invalidate it — which is most
/// of the editing anyone does.
fn fingerprint(root: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for entry in [config::FILE_NAME].iter().map(|name| root.join(name)) {
        parts.push(stamp(&entry, "hekla.toml"));
    }
    for subdir in ["events", "lib"] {
        let dir = root.join(subdir);
        if !dir.is_dir() {
            continue;
        }
        let mut findings = Vec::new();
        for path in crate::loader::star_files(&dir, &mut findings) {
            let rel = rel_to_string(root, &path);
            parts.push(stamp(&path, &rel));
        }
        // A walk error changes what was seen, so it belongs in the digest too.
        for finding in &findings {
            parts.push(format!("!{}:{}", finding.location, finding.message));
        }
    }
    crate::hash::sha256_hex(parts.join("\n").as_bytes())
}

fn stamp(path: &Path, label: &str) -> String {
    match path.metadata() {
        Ok(meta) => {
            let modified = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|since| since.as_nanos())
                .unwrap_or(0);
            format!("{label}:{}:{modified}", meta.len())
        }
        Err(_) => format!("{label}:absent"),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    /// A project with one event module and one command.
    fn project(dir: &Path) {
        fs::create_dir_all(dir.join("events")).unwrap();
        fs::create_dir_all(dir.join("commands")).unwrap();
        fs::write(
            dir.join("events/order.star"),
            "order_placed = event(type = \"order.placed\", fields = {\"order_id\": uuid()})\n",
        )
        .unwrap();
        fs::write(dir.join("commands/place-order.star"), "input = schema()\n").unwrap();
    }

    #[test]
    fn a_document_is_placed_in_its_project_and_role() {
        let dir = TempDir::new().unwrap();
        project(dir.path());
        let projects = Projects::new();

        let located = projects
            .locate(&dir.path().join("commands/place-order.star"))
            .expect("a located command");
        assert_eq!(located.root, dir.path());
        assert_eq!(located.rel, "commands/place-order.star");
        assert_eq!(located.role, Role::Command { internal: false });

        let located = projects
            .locate(&dir.path().join("events/order.star"))
            .expect("a located event module");
        assert_eq!(located.role, Role::Events);
    }

    /// `hekla.toml` is optional, so the directory convention alone has to place a
    /// document; and where it is present it should place one on its own.
    #[test]
    fn a_root_is_found_with_or_without_a_config() {
        let dir = TempDir::new().unwrap();
        project(dir.path());
        let projects = Projects::new();
        assert!(
            projects
                .locate(&dir.path().join("commands/place-order.star"))
                .is_some()
        );

        let bare = TempDir::new().unwrap();
        fs::write(bare.path().join("hekla.toml"), "").unwrap();
        fs::create_dir_all(bare.path().join("tests")).unwrap();
        fs::write(bare.path().join("tests/a.star"), "cases = []\n").unwrap();
        let projects = Projects::new();
        let located = projects
            .locate(&bare.path().join("tests/a.star"))
            .expect("config alone should place a document");
        assert_eq!(located.role, Role::Test);
    }

    #[test]
    fn the_deepest_project_wins() {
        let outer = TempDir::new().unwrap();
        project(outer.path());
        let inner = outer.path().join("examples/inner");
        project(&inner);

        let projects = Projects::new();
        let located = projects
            .locate(&inner.join("commands/place-order.star"))
            .expect("a located command");
        assert_eq!(located.root, inner);
    }

    /// A repository that merely has `tests/` and `lib/` is not a hekla project.
    /// Getting this wrong would attach hekla's builtins to unrelated Starlark, the
    /// exact failure this server exists to fix.
    #[test]
    fn an_ordinary_repository_is_not_a_project() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("tests")).unwrap();
        fs::create_dir_all(dir.path().join("lib")).unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("tests/a.star"), "").unwrap();

        let projects = Projects::new();
        assert_eq!(projects.locate(&dir.path().join("tests/a.star")), None);
    }

    #[test]
    fn a_file_outside_the_convention_is_not_placed() {
        let dir = TempDir::new().unwrap();
        project(dir.path());
        fs::write(dir.path().join("scratch.star"), "").unwrap();

        let projects = Projects::new();
        assert_eq!(projects.locate(&dir.path().join("scratch.star")), None);
    }

    /// The whole point of the fingerprint: editing a command must not invalidate
    /// the snapshot, and editing a library must.
    #[test]
    fn only_the_shared_half_moves_the_fingerprint() {
        let dir = TempDir::new().unwrap();
        project(dir.path());
        let before = fingerprint(dir.path());

        fs::write(
            dir.path().join("commands/place-order.star"),
            "input = schema(order_id = uuid())\n",
        )
        .unwrap();
        assert_eq!(
            before,
            fingerprint(dir.path()),
            "a command edit must not invalidate the snapshot"
        );

        fs::write(
            dir.path().join("events/order.star"),
            "order_placed = event(type = \"order.placed\", fields = {\"order_id\": uuid(), \"total\": money()})\n",
        )
        .unwrap();
        assert_ne!(
            before,
            fingerprint(dir.path()),
            "an event-module edit must invalidate the snapshot"
        );
    }

    #[test]
    fn a_snapshot_carries_the_projects_libraries() {
        let dir = TempDir::new().unwrap();
        project(dir.path());
        let projects = Projects::new();

        let snapshot = projects.snapshot(dir.path());
        assert!(snapshot.project.events.by_type.contains_key("order.placed"));
        assert_eq!(snapshot.project.library_paths, ["events/order.star"]);
        // The units are not loaded: that is what makes it cheap.
        assert!(snapshot.project.commands.is_empty());
    }
}
