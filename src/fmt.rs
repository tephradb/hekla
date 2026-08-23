//! `kiln fmt`: a conservative whitespace normaliser for `.star` files.
//!
//! Starlark indentation is syntactically meaningful, so this deliberately does
//! not reflow code. It normalises line endings to `\n`, strips trailing
//! whitespace, and ensures exactly one trailing newline. Those transforms never
//! change a program's meaning, so `fmt` is always safe to run. Full AST-level
//! reformatting is future work: starlark-rust 0.14 exposes no pretty-printer.

use std::fs;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::loader::rel_to_string;

/// Directories that never hold project source and are skipped when walking.
const SKIP_DIRS: [&str; 4] = [".git", "target", "kiln-data", "data"];

pub struct Outcome {
    /// Files whose contents differ from their normalised form.
    pub changed: Vec<String>,
    /// Files that could not be read or written, with the reason.
    pub errors: Vec<(String, String)>,
}

/// Normalise every `.star` file under `root`. When `check_only` is set nothing
/// is written; the outcome still lists what would change.
pub fn run(root: &Path, check_only: bool) -> Outcome {
    let mut outcome = Outcome {
        changed: Vec::new(),
        errors: Vec::new(),
    };
    for path in star_files(root) {
        let rel = rel_to_string(root, &path);
        let src = match fs::read_to_string(&path) {
            Ok(src) => src,
            Err(err) => {
                outcome.errors.push((rel, format!("reading file: {err}")));
                continue;
            }
        };
        let normalised = normalize(&src);
        if normalised == src {
            continue;
        }
        outcome.changed.push(rel.clone());
        if !check_only && let Err(err) = fs::write(&path, normalised) {
            outcome.changed.pop();
            outcome.errors.push((rel, format!("writing file: {err}")));
        }
    }
    outcome
}

/// Line endings to `\n`, trailing whitespace stripped, exactly one trailing
/// newline. Indentation is untouched.
fn normalize(src: &str) -> String {
    let unified = src.replace("\r\n", "\n").replace('\r', "\n");
    let body = unified
        .lines()
        .map(|line| line.trim_end_matches([' ', '\t']))
        .collect::<Vec<_>>()
        .join("\n");
    let body = body.trim_end_matches('\n');
    if body.is_empty() {
        String::new()
    } else {
        format!("{body}\n")
    }
}

fn star_files(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            !entry.file_type().is_dir()
                || entry
                    .file_name()
                    .to_str()
                    .is_none_or(|name| !SKIP_DIRS.contains(&name))
        })
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("star"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_trailing_whitespace_and_normalises_endings() {
        assert_eq!(normalize("a = 1  \r\nb = 2\t\n"), "a = 1\nb = 2\n");
    }

    #[test]
    fn collapses_trailing_blank_lines_to_one_newline() {
        assert_eq!(normalize("x = 1\n\n\n"), "x = 1\n");
    }

    #[test]
    fn adds_a_missing_final_newline() {
        assert_eq!(normalize("x = 1"), "x = 1\n");
    }

    #[test]
    fn leaves_indentation_untouched() {
        let src = "def f():\n    return 1\n";
        assert_eq!(normalize(src), src);
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(normalize(""), "");
        assert_eq!(normalize("\n\n"), "");
    }

    /// A file whose write fails was not formatted, so it must not be counted as
    /// one: reporting both `error:` and `formatted:` for the same path is a lie.
    #[cfg(unix)]
    #[test]
    fn a_file_that_cannot_be_written_is_not_counted_as_formatted() {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.star");
        fs::write(&path, "x = 1  \n").unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o444)).unwrap();
        if fs::write(&path, "x = 1  \n").is_ok() {
            return; // running as root, where the read-only bit denies nothing
        }

        let outcome = run(dir.path(), false);
        assert_eq!(outcome.errors.len(), 1, "{:?}", outcome.errors);
        assert!(outcome.changed.is_empty(), "{:?}", outcome.changed);
    }
}
