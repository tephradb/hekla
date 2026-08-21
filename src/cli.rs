//! The `kiln` command-line interface.
//!
//! Phase 1 ships `check` (thorough static analysis) and `fmt` (whitespace
//! normalisation). `serve` and `test` are declared so the surface is stable, but
//! land in later phases.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::loader::{Finding, LoadedProject, Severity};
use crate::{fmt, validate};

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
    /// Run the runtime and HTTP API. Lands in a later phase.
    Serve {
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Run command tests. Lands in a later phase.
    Test {
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
        Command::Serve { .. } => not_yet("serve", "phase 2"),
        Command::Test { .. } => not_yet("test", "phase 2"),
    }
}

fn check(dir: &Path) -> ExitCode {
    let project = LoadedProject::load(dir);
    let mut findings = project.findings.clone();
    findings.extend(validate::check(&project));
    findings.sort_by(|left, right| left.location.cmp(&right.location));
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

fn not_yet(name: &str, phase: &str) -> ExitCode {
    eprintln!("kiln {name} is not available yet; it lands in {phase}");
    ExitCode::from(2)
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
