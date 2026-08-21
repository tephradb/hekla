//! The `kiln` command-line interface.
//!
//! `check` (thorough static analysis) and `fmt` (whitespace normalisation) are
//! toolchain commands; `serve` runs the command runtime and HTTP API, and `test`
//! runs the command scenarios under `tests/`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::loader::{Finding, LoadedProject, Severity};
use crate::{fmt, runtime, server, testing, validate};

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
}

/// Parse arguments and run, returning the process exit code.
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Check { dir } => check(&dir),
        Command::Fmt { dir, check } => run_fmt(&dir, check),
        Command::Serve {
            dir,
            addr,
            data_dir,
        } => serve(&dir, addr.as_deref(), data_dir.as_deref()),
        Command::Test { dir } => testing::run(&dir),
    }
}

fn check(dir: &Path) -> ExitCode {
    let project = LoadedProject::load(dir);
    let findings = collect_findings(&project);
    print_findings(&findings);
    let errors = findings
        .iter()
        .filter(|finding| finding.severity == Severity::Error)
        .count();
    let warnings = findings.len() - errors;

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
    let errors = report_findings(&project);
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
    let (rt, coordinator) = match runtime::Runtime::open(project, &data) {
        Ok(pair) => pair,
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
    match tokio_rt.block_on(server::serve(rt, coordinator, addr)) {
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

/// The loader findings plus the semantic checks, sorted by location.
fn collect_findings(project: &LoadedProject) -> Vec<Finding> {
    let mut findings = project.findings.clone();
    findings.extend(validate::check(project));
    findings.sort_by(|left, right| left.location.cmp(&right.location));
    findings
}

/// Print every finding and return the error count.
fn report_findings(project: &LoadedProject) -> usize {
    let findings = collect_findings(project);
    print_findings(&findings);
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
        let severity = match finding.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        println!("{severity}: {}: {}", finding.location, finding.message);
    }
}
