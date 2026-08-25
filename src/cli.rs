//! The `kiln` command-line interface.
//!
//! `check` (thorough static analysis) and `fmt` (whitespace normalisation) are
//! toolchain commands; `serve` runs the command runtime and HTTP API, and `test`
//! runs the command scenarios under `tests/`. `rotate` and `erase` are the
//! operational key commands: rewrapping subject keys under a new master, and
//! irreversibly deleting one subject's key.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::effect::{HttpClient, UreqClient};
use crate::loader::{Finding, LoadedProject, Severity};
use crate::opdb::OpDb;
use crate::{crypto, fmt, runtime, server, testing, validate};

/// The default HTTP bind address when `--addr` is not given.
const DEFAULT_ADDR: &str = "127.0.0.1:8080";

#[derive(Parser)]
#[command(
    name = "kiln",
    version,
    about = "event-sourced runtime with Starlark modules"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Load a project and report problems without running it.
    Check {
        /// The project directory.
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Normalise whitespace in `.star` files.
    Fmt {
        /// The project directory.
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Report files that need formatting instead of writing them.
        #[arg(long)]
        check: bool,
    },
    /// Run the Starlark language server over stdio, for editor integration.
    ///
    /// Takes no project directory: each open file is placed in its own project,
    /// so one editor session can span several.
    Lsp {
        /// Accepted and ignored. Editors conventionally pass it, and stdio is the
        /// only transport.
        #[arg(long)]
        stdio: bool,
        /// Skip evaluating each file against its project, leaving parsing, load
        /// rules and name resolution.
        #[arg(long)]
        no_project_checks: bool,
    },
    /// Run the runtime and HTTP API from a project directory.
    Serve {
        /// The project directory.
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// The HTTP bind address.
        #[arg(long)]
        addr: Option<String>,
        /// The data directory (event store and operational DB). Defaults to
        /// `<dir>/data`.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Run the command scenarios under `tests/`.
    Test {
        /// The project directory.
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Rewrap every subject key under the primary master key (`KILN_MASTER_KEY`),
    /// unwrapping with the previous keys (`KILN_MASTER_KEY_PREVIOUS`) as needed. Run
    /// after changing the master to migrate rows off the old key. Ciphertext is
    /// unchanged, so reads keep working throughout.
    Rotate {
        /// The project directory (to resolve the data directory).
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// The data directory (operational DB). Defaults to `<dir>/data`.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Erase a subject: delete its encryption key, making every value scoped to it
    /// unreadable and unmatchable across the log and every read model at once. This
    /// is irreversible.
    Erase {
        /// The subject field (e.g. `customer_id`).
        subject_field: String,
        /// The subject id value (e.g. `42`).
        subject_value: String,
        /// The project directory (to resolve the data directory).
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// The data directory (operational DB). Defaults to `<dir>/data`.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
}

/// Parse arguments and run, returning the process exit code.
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Check { dir } => check(&dir),
        Command::Fmt { dir, check } => run_fmt(&dir, check),
        Command::Lsp {
            stdio: _,
            no_project_checks,
        } => crate::lsp::run(!no_project_checks),
        Command::Serve {
            dir,
            addr,
            data_dir,
        } => serve(&dir, addr.as_deref(), data_dir.as_deref()),
        Command::Test { dir } => testing::run(&dir),
        Command::Rotate { dir, data_dir } => rotate(&dir, data_dir.as_deref()),
        Command::Erase {
            subject_field,
            subject_value,
            dir,
            data_dir,
        } => erase(&subject_field, &subject_value, &dir, data_dir.as_deref()),
    }
}

/// Rewrap every subject key under the primary master. Needs `KILN_MASTER_KEY` (and
/// `KILN_MASTER_KEY_PREVIOUS` for the keys rows are currently wrapped under).
fn rotate(dir: &Path, data_dir: Option<&Path>) -> ExitCode {
    let master = match crypto::master_keys_from_env() {
        Ok(Some(master)) => master,
        Ok(None) => {
            eprintln!("error: KILN_MASTER_KEY must be set to rotate");
            return ExitCode::FAILURE;
        }
        Err(err) => {
            eprintln!("error: reading the master key: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    let db_path = match operational_db(dir, data_dir) {
        Ok(path) => path,
        Err(code) => return code,
    };
    let opdb = match OpDb::open(&db_path) {
        Ok(opdb) => Arc::new(Mutex::new(opdb)),
        Err(err) => {
            eprintln!("error: opening the operational database: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    let keystore = crypto::KeyStore::new(opdb, master);
    match keystore.rotate() {
        Ok(count) => {
            println!("rewrapped {count} subject key(s) under the primary master");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Erase a subject by deleting its key from the operational DB. No master key is
/// needed: this is a row delete that shreds the ciphertext everywhere at once.
fn erase(
    subject_field: &str,
    subject_value: &str,
    dir: &Path,
    data_dir: Option<&Path>,
) -> ExitCode {
    let db_path = match operational_db(dir, data_dir) {
        Ok(path) => path,
        Err(code) => return code,
    };
    let opdb = match OpDb::open(&db_path) {
        Ok(opdb) => opdb,
        Err(err) => {
            eprintln!("error: opening the operational database: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    match crypto::erase_subject(&opdb, subject_field, subject_value) {
        Ok(true) => {
            println!("erased subject `{subject_field}` = `{subject_value}`");
            ExitCode::SUCCESS
        }
        Ok(false) => {
            println!(
                "no key for subject `{subject_field}` = `{subject_value}` (already erased or never created)"
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// The operational database inside the resolved data directory. `OpDb::open`
/// creates the file when it is missing, so a mistyped `--data-dir` would otherwise
/// let `rotate` and `erase` report success against a fresh empty database.
fn operational_db(dir: &Path, data_dir: Option<&Path>) -> Result<PathBuf, ExitCode> {
    let path = runtime::resolve_data_dir(dir, data_dir).join("kiln.db");
    if path.exists() {
        Ok(path)
    } else {
        eprintln!("error: no operational database at {}", path.display());
        Err(ExitCode::FAILURE)
    }
}

fn check(dir: &Path) -> ExitCode {
    let project = LoadedProject::load(dir);
    let (errors, warnings) = report_findings(&project);

    let modules = project.commands.len() + project.projectors.len() + project.effects.len();
    println!(
        "\nchecked {modules} module(s): {} command(s), {} projector(s), {} effect(s), {} event(s)",
        project.commands.len(),
        project.projectors.len(),
        project.effects.len(),
        project.events.by_type.len(),
    );
    if errors == 0 {
        println!("ok: no errors, {warnings} warning(s)");
        ExitCode::SUCCESS
    } else {
        println!("failed: {errors} error(s), {warnings} warning(s)");
        ExitCode::FAILURE
    }
}

fn serve(dir: &Path, addr: Option<&str>, data_dir: Option<&Path>) -> ExitCode {
    init_tracing();

    let project = LoadedProject::load(dir);
    let (errors, _) = report_findings(&project);
    if errors > 0 {
        eprintln!("refusing to serve: the project has {errors} error(s)");
        return ExitCode::FAILURE;
    }

    let addr: SocketAddr = match addr.unwrap_or(DEFAULT_ADDR).parse() {
        Ok(addr) => addr,
        Err(err) => {
            eprintln!("error: invalid --addr: {err}");
            return ExitCode::FAILURE;
        }
    };
    let data = runtime::resolve_data_dir(dir, data_dir);
    let http: Arc<dyn HttpClient> = Arc::new(UreqClient::new());
    let master = match crypto::master_keys_from_env() {
        Ok(master) => master,
        Err(err) => {
            eprintln!("error: reading the master key: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    let (rt, coordinator, projectors, effects) =
        match runtime::Runtime::open(project, &data, http, master) {
            Ok(parts) => parts,
            Err(err) => {
                eprintln!("error: {err:#}");
                return ExitCode::FAILURE;
            }
        };

    let tokio_rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(tokio_rt) => tokio_rt,
        Err(err) => {
            eprintln!("error: building the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    match tokio_rt.block_on(server::serve(rt, coordinator, projectors, effects, addr)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run_fmt(dir: &Path, check_only: bool) -> ExitCode {
    let outcome = fmt::run(dir, check_only);
    for (path, err) in &outcome.errors {
        eprintln!("error: {path}: {err}");
    }
    if check_only {
        for path in &outcome.changed {
            println!("would format: {path}");
        }
        if !outcome.errors.is_empty() {
            ExitCode::FAILURE
        } else if outcome.changed.is_empty() {
            println!("ok: all files formatted");
            ExitCode::SUCCESS
        } else {
            println!("{} file(s) need formatting", outcome.changed.len());
            ExitCode::FAILURE
        }
    } else {
        for path in &outcome.changed {
            println!("formatted: {path}");
        }
        if outcome.errors.is_empty() {
            println!("formatted {} file(s)", outcome.changed.len());
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    }
}

/// The loader findings plus the semantic checks, sorted by location. Shared with
/// `kiln test` so every command reports the same findings in the same order.
pub(crate) fn collect_findings(project: &LoadedProject) -> Vec<Finding> {
    let mut findings = project.findings.clone();
    findings.extend(validate::check(project));
    findings.sort_by(|left, right| {
        let position = |finding: &Finding| {
            finding
                .span
                .map(|span| (span.begin.line, span.begin.column))
        };
        left.location
            .cmp(&right.location)
            .then_with(|| position(left).cmp(&position(right)))
    });
    findings
}

/// Print every finding and return the (error, warning) counts.
fn report_findings(project: &LoadedProject) -> (usize, usize) {
    let findings = collect_findings(project);
    print_findings(&findings);
    let errors = findings
        .iter()
        .filter(|finding| finding.severity == Severity::Error)
        .count();
    (errors, findings.len() - errors)
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn print_findings(findings: &[Finding]) {
    for finding in findings {
        let severity = match finding.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        // Spans are 0-based; editors and humans count from one.
        let at = match finding.span {
            Some(span) => format!(":{}:{}", span.begin.line + 1, span.begin.column + 1),
            None => String::new(),
        };
        println!("{severity}: {}{at}: {}", finding.location, finding.message);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// `OpDb::open` would create the database, so without this guard `kiln erase`
    /// against a mistyped `--data-dir` reports a successful no-op erasure.
    #[test]
    fn a_data_dir_with_no_database_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(operational_db(dir.path(), None).is_err());

        let data = dir.path().join("data");
        fs::create_dir_all(&data).unwrap();
        let db_path = data.join("kiln.db");
        fs::write(&db_path, b"").unwrap();
        assert_eq!(operational_db(dir.path(), None).unwrap(), db_path);
    }
}
