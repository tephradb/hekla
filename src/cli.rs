//! The `hekla` command-line interface.
//!
//! `check` (thorough static analysis) and `fmt` (whitespace normalisation) are
//! toolchain commands; `serve` runs the command runtime and HTTP API, and `test`
//! runs the scenarios under `tests/`. `rotate` and `erase` are the
//! operational key commands: rewrapping subject keys under a new master, and
//! irreversibly deleting one subject's key.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::http::{HttpClient, UreqClient};
use crate::loader::{Finding, LoadedProject, Severity};
use crate::opdb::OpDb;
use crate::{crypto, runtime, server, testing, validate};

/// The default HTTP bind address when `--addr` is not given.
const DEFAULT_ADDR: &str = "127.0.0.1:8080";

#[derive(Parser)]
#[command(
    name = "hekla",
    version,
    about = "event-sourced runtime with heklang modules"
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
        /// Run the continuous invariant check: every completed effect invocation is
        /// replayed against a sealed journal. An effect that breaks the invariant is
        /// quarantined.
        #[arg(long)]
        verify: bool,
    },
    /// Run the scenarios under `tests/`, covering commands, projectors and effects.
    Test {
        /// The project directory.
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Check the invariants the design rests on against a data directory: that a
    /// projector rebuilt from position 0 matches the live one, and that every
    /// recorded effect invocation still replays without performing anything.
    ///
    /// Takes the data-directory lock, so it refuses to run against a directory a
    /// server has open. Verify a copy of the directory, which checks the backup at
    /// the same time.
    Verify {
        /// The project directory.
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// The data directory (event store and operational DB). Defaults to
        /// `<dir>/data`.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Rewrap every subject key under the primary master key (`HEKLA_MASTER_KEY`),
    /// unwrapping with the previous keys (`HEKLA_MASTER_KEY_PREVIOUS`) as needed. Run
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
    /// Print the generated OpenAPI 3.1 document for a project to stdout.
    ///
    /// Reads the project only: no data directory, no lock, and no master key, so it
    /// runs anywhere `hekla check` does. The document is the same one a running
    /// server serves at `/openapi.json`, pretty-printed here so a committed
    /// `openapi.json` diffs by line and an unintended API change shows up in CI.
    ///
    /// Findings go to stderr, so `hekla openapi . > openapi.json` writes only JSON.
    Openapi {
        /// The project directory.
        #[arg(default_value = ".")]
        dir: PathBuf,
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
        Command::Serve {
            dir,
            addr,
            data_dir,
            verify,
        } => serve(&dir, addr.as_deref(), data_dir.as_deref(), verify),
        Command::Test { dir } => testing::run(&dir),
        Command::Verify { dir, data_dir } => verify(&dir, data_dir.as_deref()),
        Command::Rotate { dir, data_dir } => rotate(&dir, data_dir.as_deref()),
        Command::Openapi { dir } => openapi(&dir),
        Command::Erase {
            subject_field,
            subject_value,
            dir,
            data_dir,
        } => erase(&subject_field, &subject_value, &dir, data_dir.as_deref()),
    }
}

/// Print the generated OpenAPI document for a project.
///
/// The only subcommand that writes its findings to stderr: stdout is the document,
/// and `hekla openapi . > openapi.json` has to produce a file `jq` will parse.
fn openapi(dir: &Path) -> ExitCode {
    // `LoadedProject::load` reports no findings for a root that does not exist or holds
    // no modules: it discovers nothing and succeeds at it. Every other subcommand can
    // afford that (`check` says "checked 0 module(s)" and moves on), but this one's
    // output gets committed, so a typo'd path or a run from the wrong working directory
    // would overwrite a real spec with a six-path stub and exit 0.
    if !dir.is_dir() {
        eprintln!("error: `{}` is not a directory", dir.display());
        return ExitCode::FAILURE;
    }
    let project = LoadedProject::load(dir);
    let findings = collect_findings(&project);
    for finding in &findings {
        eprintln!("{}", render_finding(finding));
    }
    let errors = count_errors(&findings);
    if errors > 0 {
        eprintln!("refusing to generate: the project has {errors} error(s)");
        return ExitCode::FAILURE;
    }
    if project.commands.is_empty()
        && project.projectors.is_empty()
        && project.effects.is_empty()
        && project.events.is_empty()
    {
        eprintln!(
            "error: `{}` declares no commands, projectors, effects or events, so there is \
             nothing to describe; is this a hekla project directory?",
            dir.display()
        );
        return ExitCode::FAILURE;
    }
    let document = crate::openapi::build(&crate::openapi::Surface::from_project(&project));
    match serde_json::to_string_pretty(&document) {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: serializing the document: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Rewrap every subject key under the primary master. Needs `HEKLA_MASTER_KEY` (and
/// `HEKLA_MASTER_KEY_PREVIOUS` for the keys rows are currently wrapped under).
fn rotate(dir: &Path, data_dir: Option<&Path>) -> ExitCode {
    let master = match crypto::master_keys_from_env() {
        Ok(Some(master)) => master,
        Ok(None) => {
            eprintln!("error: HEKLA_MASTER_KEY must be set to rotate");
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
    let path = runtime::resolve_data_dir(dir, data_dir).join("hekla.db");
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
        project.events.len(),
    );
    if errors == 0 {
        println!("ok: no errors, {warnings} warning(s)");
        ExitCode::SUCCESS
    } else {
        println!("failed: {errors} error(s), {warnings} warning(s)");
        ExitCode::FAILURE
    }
}

/// `hekla verify`: the offline invariant sweep over a data directory.
///
/// Exits non-zero on any violation, so it drops straight into CI or a nightly job.
/// It reports what it checked even when clean, because a sweep that found nothing
/// because it covered nothing must not read like a passing one.
fn verify(dir: &Path, data_dir: Option<&Path>) -> ExitCode {
    init_tracing();

    let project = LoadedProject::load(dir);
    let (errors, _) = report_findings(&project);
    if errors > 0 {
        eprintln!("refusing to verify: the project has {errors} error(s)");
        return ExitCode::FAILURE;
    }
    let data = runtime::resolve_data_dir(dir, data_dir);
    if !data.exists() {
        eprintln!("error: no data directory at {}", data.display());
        return ExitCode::FAILURE;
    }
    let master = match crypto::master_keys_from_env() {
        Ok(master) => master,
        Err(err) => {
            eprintln!("error: reading the master key: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    match crate::verify::sweep(&project, &data, master) {
        Ok(report) => {
            println!("{report}");
            if report.is_clean() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
fn serve(dir: &Path, addr: Option<&str>, data_dir: Option<&Path>, verify: bool) -> ExitCode {
    init_tracing();

    let mut project = LoadedProject::load(dir);
    let (errors, _) = report_findings(&project);
    if errors > 0 {
        eprintln!("refusing to serve: the project has {errors} error(s)");
        return ExitCode::FAILURE;
    }
    // The flag turns the checks on without editing `hekla.toml`; the file can turn
    // them on permanently. Neither can turn the other off, so `--verify` on a
    // project that already enables them is a no-op rather than a surprise.
    if verify {
        project.config.verify.enabled = true;
    }
    if project.config.verify.enabled {
        tracing::info!("verify mode on: effect replays are checked as they run");
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

/// The loader findings plus the semantic checks, sorted by location. Shared with
/// `hekla test` so every command reports the same findings in the same order.
pub(crate) fn collect_findings(project: &LoadedProject) -> Vec<Finding> {
    let mut findings = project.findings.clone();
    findings.extend(validate::check(project));
    findings.sort_by(|left, right| {
        let position = |finding: &Finding| finding.span.map(|span| (span.line, span.column));
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
    let errors = count_errors(&findings);
    (errors, findings.len() - errors)
}

/// How many findings are errors, which is what every load-and-refuse path branches on.
/// Shared with `openapi`, which reports to stderr instead and so cannot use
/// [`report_findings`] wholesale.
fn count_errors(findings: &[Finding]) -> usize {
    findings
        .iter()
        .filter(|finding| finding.severity == Severity::Error)
        .count()
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn print_findings(findings: &[Finding]) {
    for finding in findings {
        println!("{}", render_finding(finding));
    }
}

/// One finding as a line. Shared with `hekla openapi`, which writes the same lines to
/// stderr so its stdout stays parseable JSON.
pub(crate) fn render_finding(finding: &Finding) -> String {
    let severity = match finding.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
    };
    // Spans are 0-based; editors and humans count from one.
    let at = match finding.span {
        Some(span) => format!(":{}:{}", span.line + 1, span.column + 1),
        None => String::new(),
    };
    let line = format!("{severity}: {}{at}: {}", finding.location, finding.message);
    // heklang carries the fix on a separate hint, and a diagnostic that names the
    // problem without it is the worse half of the message.
    match &finding.hint {
        Some(hint) => format!("{line}\n  = {hint}"),
        None => line,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// `OpDb::open` would create the database, so without this guard `hekla erase`
    /// against a mistyped `--data-dir` reports a successful no-op erasure.
    #[test]
    fn a_data_dir_with_no_database_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(operational_db(dir.path(), None).is_err());

        let data = dir.path().join("data");
        fs::create_dir_all(&data).unwrap();
        let db_path = data.join("hekla.db");
        fs::write(&db_path, b"").unwrap();
        assert_eq!(operational_db(dir.path(), None).unwrap(), db_path);
    }
}
