//! The `hekla` command-line interface.
//!
//! `check` (thorough static analysis) and `fmt` (whitespace normalisation) are
//! toolchain commands; `serve` runs the command runtime and HTTP API, and `test`
//! runs the scenarios under `tests/`. `rotate` and `erase` are the
//! operational key commands: rewrapping subject keys under a new master, and
//! irreversibly deleting one subject's key.

use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::http::{HttpClient, UreqClient};
use heklang::ir::Delivery;

use crate::loader::{ArmRef, EffectUnit, Finding, LoadedProject, Scratch, Sources};
use crate::opdb::{self, OpDb};
use crate::plan::Replay;
use crate::progress::Progress;
use crate::{crypto, lock, metrics, projection, runtime, server, testing, validate};

/// The default HTTP bind address when `--addr` is not given.
const DEFAULT_ADDR: &str = "127.0.0.1:8080";

/// The tracing filter when `RUST_LOG` is unset or does not parse.
const DEFAULT_LOG_FILTER: &str = "info";

#[derive(Parser)]
#[command(
    name = "hekla",
    version,
    about = "event-sourced runtime with heklang modules"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Disable ANSI colors in the log output. Colors are off anyway when the output is
    /// not a terminal, or when `NO_COLOR` is set to a non-empty value.
    ///
    /// Global, so it is accepted both before and after the subcommand.
    #[arg(long, global = true)]
    no_color: bool,
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
    /// Move every subject key under the parent its subject declares, for rows minted
    /// before the parent was declared.
    ///
    /// `hekla serve` does this at boot and refuses to serve without it, so this exists to
    /// run the work *ahead* of a deploy: point it at the new project and the live data
    /// directory, and the boot that follows finds nothing left to do. Takes no lock and
    /// changes no ciphertext, so a running server is unaffected.
    Adopt {
        /// The project directory, whose declarations say where each key belongs.
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// The data directory (event log and operational DB). Defaults to `<dir>/data`.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Do not draw the progress line.
        #[arg(long)]
        no_progress: bool,
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
    /// Report what deploying this project over a data directory would change: which
    /// declarations were added, removed, or now do something different, and which
    /// projectors would rebuild. With `--replay`, also what it would *do*.
    ///
    /// Reads only, and takes no data-directory lock, so unlike `verify` it runs against
    /// a directory a server has open. Without `--replay` it opens no event log at all.
    /// Exits zero whether or not anything would change; a change is the answer, not a
    /// fault.
    Plan {
        /// The project directory.
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// The data directory (event store and operational DB). Defaults to
        /// `<dir>/data`.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Print the plan as JSON on stdout, for a deploy gate to read.
        #[arg(long)]
        json: bool,
        /// Also re-run recorded effect invocations against the candidate code and report
        /// the ones it would not reproduce. Reads the event log through a read-only
        /// follower, so it still runs against a live deployment. An effect that reveals
        /// needs HEKLA_MASTER_KEY, and is reported as unreplayable without one.
        #[arg(long)]
        replay: bool,
        /// How many of each effect's most recent invocations `--replay` re-runs. What
        /// the cap drops is named in the report rather than dropped quietly.
        ///
        /// At least one: a cap of zero would replay nothing while reporting every
        /// effect as capped, which is a coverage number that describes no work at all.
        /// Requires `--replay`, for the same reason: a cap on a replay that is not
        /// happening is a request this command would otherwise accept and drop.
        #[arg(long, requires = "replay", default_value_t = crate::plan::DEFAULT_REPLAY_LIMIT,
              value_parser = clap::value_parser!(u32).range(1..))]
        replay_limit: u32,
    },
    /// Fold an ad-hoc projector over the event log and print its rows, deploying nothing.
    ///
    /// FILE is a standalone `.hk` file declaring one `projector`. It is compiled together
    /// with the project, so it typechecks against the deployed events and can call the
    /// project's own `fn`, `const`, `record` and `enum` declarations, but it is folded
    /// into a read model in a temporary directory and nothing is kept: no declaration
    /// row, no database under `data/projectors/`, no checkpoint and no route.
    ///
    /// This is for the question a deploy should not have to answer: count these, grouped
    /// by that, without waiting for a rebuild to find out. It reads the log through a
    /// read-only follower and takes no data-directory lock, so like `plan` it runs
    /// against a deployment that is serving traffic, and the rows are a snapshot at the
    /// tip pinned when it opened. Exits zero whenever the projection ran; the rows are
    /// the answer, not a fault.
    Project {
        /// The `.hk` file declaring the projector to fold. Never part of the project,
        /// and never written to.
        file: PathBuf,
        /// The project directory, whose events the projector is compiled against.
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// The data directory (event store and operational DB). Defaults to
        /// `<dir>/data`.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Which projector to fold, when FILE declares more than one. A file declaring
        /// exactly one needs no flag; one declaring none is an error, because there is
        /// nothing to fold.
        #[arg(long)]
        projector: Option<String>,
        /// Report only this entity. The default reports every one the projector
        /// declares, in declaration order.
        #[arg(long)]
        entity: Option<String>,
        /// Fold only from this position. The default folds the whole log.
        ///
        /// A narrowed window narrows the answer rather than speeding it up: a row whose
        /// creating event is below it is missing, and an accumulated one is partial. The
        /// summary names the window whenever it is not the whole log, so a partial answer
        /// cannot be read as a total.
        #[arg(long)]
        from: Option<u64>,
        /// Stop at this position instead of the log's tip, for reproducing what a
        /// projector would have shown at a moment whose position you already know.
        #[arg(long)]
        upto: Option<u64>,
        /// Stop after folding this many matching events, whatever the window says.
        ///
        /// For a first look at a log too large to fold whole. At least one: a budget of
        /// zero would fold nothing while reporting a budget, which describes no work at
        /// all. A run that spends its budget says so on its own line and never reads like
        /// a complete one.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        max_events: Option<u64>,
        /// How many rows of each entity to print. The rest are counted, never dropped
        /// quietly, and `--json` is bounded the same way for the same reason: a
        /// projection over a large log can build more rows than either form can carry.
        #[arg(long, default_value_t = crate::projection::DEFAULT_ROWS as u32,
              value_parser = clap::value_parser!(u32).range(1..))]
        rows: u32,
        /// Print sealed columns as the ciphertext that is stored rather than opening
        /// them.
        ///
        /// The fold still holds the master key, because a projector re-seals a column
        /// under the column's own field name and reads its own stored loads back as
        /// plaintext. This withholds only the last step, the way
        /// `/admin/events?decrypt=false` does, so what prints is what is on disk.
        #[arg(long)]
        no_decrypt: bool,
        /// Suppress the progress line even when stderr is a terminal.
        #[arg(long)]
        no_progress: bool,
        /// Print the projection as JSON on stdout, for a script to read.
        #[arg(long)]
        json: bool,
    },
    /// Report every deployment credential the project declares and whether this machine
    /// can supply it, without printing any of them.
    ///
    /// Reads the environment and `[secrets]` in hekla.toml, and nothing else: no data
    /// directory, no log, no lock. Exits non-zero when a required credential is unset, so
    /// it stands on its own as a pre-deploy gate for the thing `hekla serve` would
    /// otherwise refuse to start over.
    Secrets {
        /// The project directory.
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Erase a subject: delete its encryption key, making every value scoped to it
    /// unreadable and unmatchable across the log and every read model at once. This
    /// is irreversible.
    Erase {
        /// The subject, by its declared name (e.g. `Customer`).
        subject: String,
        /// The subject id value (e.g. `42`).
        subject_value: String,
        /// The project directory (to resolve the data directory).
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// The data directory (operational DB). Defaults to `<dir>/data`.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Skip the confirmation prompt. The summary is still printed.
        #[arg(long)]
        yes: bool,
    },
    /// Move an effect back to a position, so it reprocesses everything after it and
    /// performs those side effects again. This is irreversible.
    ///
    /// The only way back to history for an `on live` arm, whose boundary is already
    /// persisted: flipping the arm to `on` and redeploying does not help. Deliberately not
    /// an HTTP endpoint. An effect declaring `on live` is by definition one whose author
    /// said history must not fire, and those are exactly the effects where an accidental
    /// rewind re-sends every notification the log has ever seen.
    ///
    /// Refuses while a server holds the data directory, so it runs against a stopped
    /// process. It prints what it would discard and asks before doing it; `--yes` answers
    /// the prompt without silencing the summary.
    Rewind {
        /// The effect to rewind.
        effect: String,
        /// The position to resume strictly after. `0` reprocesses the whole log.
        position: u64,
        /// The project directory (to resolve the data directory).
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// The data directory (event store and operational DB). Defaults to `<dir>/data`.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Also lower the `on live` boundary, so `live` arms fire for history too.
        ///
        /// Off by default, and the flag is the point: the author of an `on live` arm
        /// declared that history is not news, so overriding that should be something you
        /// typed rather than something a rewind did on your behalf. It is permanent:
        /// first activation is not re-resolved on a later boot.
        #[arg(long)]
        live: bool,
        /// Skip the confirmation prompt. The summary is still printed.
        #[arg(long)]
        yes: bool,
    },
}

/// Parse arguments and run, returning the process exit code.
pub fn run() -> ExitCode {
    let Cli { command, no_color } = Cli::parse();
    // Only the two long-running commands log; `openapi` and `plan --json` write
    // machine-readable stdout that a subscriber would interleave with.
    match command {
        Command::Check { dir } => check(&dir),
        Command::Serve {
            dir,
            addr,
            data_dir,
            verify,
        } => {
            init_tracing(no_color);
            serve(&dir, addr.as_deref(), data_dir.as_deref(), verify)
        }
        Command::Test { dir } => testing::run(&dir),
        Command::Verify { dir, data_dir } => {
            init_tracing(no_color);
            verify(&dir, data_dir.as_deref())
        }
        Command::Rotate { dir, data_dir } => rotate(&dir, data_dir.as_deref()),
        Command::Adopt {
            dir,
            data_dir,
            no_progress,
        } => adopt(&dir, data_dir.as_deref(), !no_progress),
        Command::Openapi { dir } => openapi(&dir),
        Command::Plan {
            dir,
            data_dir,
            json,
            replay,
            replay_limit,
        } => plan(&dir, data_dir.as_deref(), json, replay, replay_limit),
        Command::Project {
            file,
            dir,
            data_dir,
            projector,
            entity,
            from,
            upto,
            max_events,
            rows,
            no_decrypt,
            no_progress,
            json,
        } => project(
            &file,
            &dir,
            data_dir.as_deref(),
            Ask {
                projector: projector.as_deref(),
                entity: entity.as_deref(),
                from,
                upto,
                max_events,
                rows: rows as usize,
                decrypt: !no_decrypt,
                progress: !no_progress,
                json,
            },
        ),
        Command::Secrets { dir } => secrets(&dir),
        Command::Erase {
            subject,
            subject_value,
            dir,
            data_dir,
            yes,
        } => erase(&subject, &subject_value, &dir, data_dir.as_deref(), yes),
        Command::Rewind {
            effect,
            position,
            dir,
            data_dir,
            live,
            yes,
        } => rewind(&effect, position, &dir, data_dir.as_deref(), live, yes),
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
    let findings = validate::findings(&project);
    for finding in &findings {
        eprintln!("{}", validate::render(finding));
    }
    let errors = validate::errors(&findings);
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

/// Erase a subject by deleting its key from the operational DB, after saying what that
/// costs.
///
/// No master key is needed, and that survives the hierarchy deliberately: this is a row
/// delete, and the descendant count below is a walk of parent pointers that unwraps
/// nothing. Reachability here is structural rather than cryptographic, so the whole
/// command still works on a machine that holds no key material.
///
/// **It prompts now, where it used to not.** The rationale for the old asymmetry was that
/// an erase carried its blast radius in its own arguments, because you named the subject.
/// A subject with children makes that false: `hekla erase Shop 7` says nothing about the
/// fifty thousand customers under it, which is exactly what `hekla rewind SendWelcome 0`
/// said nothing about. The asymmetry went when the reason for it did.
fn erase(
    subject: &str,
    subject_value: &str,
    dir: &Path,
    data_dir: Option<&Path>,
    yes: bool,
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
    let beneath = match opdb.descendant_key_count(subject, subject_value) {
        Ok(count) => count,
        Err(err) => {
            eprintln!("error: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    let present = match opdb.subject_key_exists(subject, subject_value) {
        Ok(present) => present,
        Err(err) => {
            eprintln!("error: {err:#}");
            return ExitCode::FAILURE;
        }
    };

    println!("subject `{subject}` = `{subject_value}`");
    if present {
        println!("  key            present, and about to be deleted");
    } else {
        println!("  key            already absent (erased, or never created)");
    }
    match beneath {
        0 => println!("  beneath it     no other keys"),
        // Named as what it costs rather than as a row count: the rows stay, and what
        // actually happens to them is that nothing can open them again.
        n => println!("  beneath it     {n} key(s), which become permanently unreadable"),
    }
    println!();
    println!("This is irreversible. Every value scoped to these keys becomes unreadable");
    println!("across the log and every read model at once.");

    if !yes {
        // A prompt nobody can answer is a usage error, not a decline. Exiting zero here
        // would tell a script the erasure happened.
        if !io::stdin().is_terminal() {
            eprintln!("error: not a terminal; pass --yes to confirm without a prompt");
            return ExitCode::FAILURE;
        }
        if !confirm() {
            println!("nothing was erased");
            return ExitCode::SUCCESS;
        }
    }

    match crypto::erase_subject(&opdb, subject, subject_value) {
        Ok(true) => {
            println!("erased subject `{subject}` = `{subject_value}`");
            ExitCode::SUCCESS
        }
        Ok(false) => {
            println!(
                "no key for subject `{subject}` = `{subject_value}` (already erased or never created)"
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Move an effect's watermark backwards so it reprocesses, after saying what that costs.
///
/// `hekla rewind SendWelcome 0` tells you nothing about the four hundred emails it is
/// about to re-send, which is why it prints a summary and asks. `--yes` suppresses only
/// the question and never the summary. `erase` reads the same way now, for the same
/// reason: naming a subject stopped bounding the blast radius once one could have
/// children.
fn rewind(
    effect: &str,
    position: u64,
    dir: &Path,
    data_dir: Option<&Path>,
    live: bool,
    yes: bool,
) -> ExitCode {
    let project = LoadedProject::load(dir);
    if report_findings(&project).0 > 0 {
        eprintln!("error: the project does not load, so its arms cannot be named");
        return ExitCode::FAILURE;
    }
    let Some(unit) = project
        .effects
        .iter()
        .find(|unit| unit.def.name() == effect)
    else {
        eprintln!("error: no effect `{effect}` in {}", dir.display());
        let known: Vec<&str> = project.effects.iter().map(|unit| unit.def.name()).collect();
        eprintln!("       declared here: {}", render_list(&known));
        return ExitCode::FAILURE;
    };

    let db_path = match operational_db(dir, data_dir) {
        Ok(path) => path,
        Err(code) => return code,
    };
    // The lock is the "is a server running?" check. A rewind against a live process would
    // race its in-memory mark and be overwritten by the next publish, so it would appear
    // to apply and quietly not.
    let resolved = runtime::resolve_data_dir(dir, data_dir);
    let _lock = match lock::DataDirLock::acquire(&resolved) {
        Ok(lock) => lock,
        Err(err) => {
            eprintln!("error: {err:#}");
            eprintln!("       a rewind runs against a stopped process; stop the server first");
            return ExitCode::FAILURE;
        }
    };
    let mut opdb = match OpDb::open(&db_path) {
        Ok(opdb) => opdb,
        Err(err) => {
            eprintln!("error: opening the operational database: {err:#}");
            return ExitCode::FAILURE;
        }
    };

    // Checked before anything about the directory's state, so a stale runbook flag is
    // reported even when the rewind itself turns out to be a no-op.
    if live && !unit.arms.values().any(|arm| arm.delivery == Delivery::Live) {
        eprintln!("error: effect `{effect}` has no `on live` arm, so --live would do nothing");
        return ExitCode::FAILURE;
    }

    let watermark = match opdb.effect_resume_after(effect) {
        Ok(watermark) => watermark,
        Err(err) => {
            eprintln!("error: reading the effect cursor: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    // Strictly greater. Rewinding *to* the current watermark is a real operation and the
    // one a `blocked` effect needs: it discards every lane row and every recorded
    // invocation above the mark without moving the mark itself. Refusing it made the
    // escape hatch the block message names a no-op, and unrecoverable at watermark 0.
    if position > watermark {
        println!(
            "effect `{effect}` is at position {watermark}; {position} is ahead of it, not a rewind"
        );
        return ExitCode::SUCCESS;
    }
    let counts = match opdb.rewind_preview(effect, position) {
        Ok(counts) => counts,
        Err(err) => {
            eprintln!("error: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    let boundary = match opdb.effect_activation(effect) {
        Ok(activation) => activation.map(|activation| activation.live_boundary),
        Err(err) => {
            eprintln!("error: reading the effect activation: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    print_rewind(effect, position, watermark, boundary, live, unit, &counts);
    if !yes {
        // A prompt nobody can answer is a usage error, not a decline. Exiting zero here
        // would tell a script the rewind happened.
        if !io::stdin().is_terminal() {
            eprintln!("error: not a terminal; pass --yes to confirm without a prompt");
            return ExitCode::FAILURE;
        }
        if !confirm() {
            println!("nothing was rewound");
            return ExitCode::SUCCESS;
        }
    }
    match opdb.rewind_effect(effect, position, live) {
        Ok(_) => {
            println!("rewound `{effect}` to position {position}; start the server to reprocess");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// What the rewind would do, named arm by arm.
///
/// A count alone is not enough on a mixed effect: knowing that nine of the positions
/// belong to an `on live` arm, and are therefore about to fire or not depending on one
/// flag, is the whole of what an operator needs before answering the prompt.
fn print_rewind(
    effect: &str,
    to: u64,
    watermark: u64,
    boundary: Option<u64>,
    live: bool,
    unit: &EffectUnit,
    counts: &opdb::RewindCounts,
) {
    println!("effect `{effect}`");
    println!("  watermark      {watermark} -> {to}");
    match boundary {
        Some(boundary) if live => println!("  live boundary  {boundary} -> {to}"),
        Some(boundary) => {
            println!("  live boundary  {boundary} (unchanged; pass --live to lower it)")
        }
        None => println!("  live boundary  unresolved; this effect has never run here"),
    }
    println!(
        "  discards       {} recorded invocation(s), and the journal rows behind them",
        counts.invocations
    );
    if counts.lanes > 0 {
        println!(
            "  lanes          {} row(s) of per-lane progress",
            counts.lanes
        );
    }
    if counts.quarantine {
        println!("  quarantine     would be cleared");
    }
    println!("  arms");
    let mut arms: Vec<(&String, &ArmRef)> = unit.arms.iter().collect();
    arms.sort_by_key(|(ty, _)| (*ty).clone());
    for (ty, arm) in arms {
        let modifier = match arm.delivery {
            Delivery::Live => "on live",
            Delivery::Latest => "on latest",
            Delivery::Every => "on",
        };
        let fate = match (arm.delivery, live) {
            (Delivery::Live, false) => "  still declined below the boundary",
            (Delivery::Live, true) => "  will fire for history",
            // Worth saying, because the invocation count above is the number of records
            // discarded and this arm will not want that many back: it re-collapses, so the
            // history it re-runs costs one invocation per key per batch, not one per event.
            (Delivery::Latest, _) => "  re-runs once per key, not once per position",
            (Delivery::Every, _) => "",
        };
        println!(
            "                 {modifier} @{ty} {{ @key {} }}{fate}",
            arm.keys.join(", ")
        );
    }
    println!();
    println!("This re-runs those positions and performs their side effects again.");
}

/// Ask. Only ever called on a terminal: an irreversible thing that could be armed by a
/// piped `yes` is one waiting to happen in a script nobody read, so each caller refuses
/// outright rather than reading an answer from a pipe.
fn confirm() -> bool {
    print!("Continue? [y/N] ");
    let _ = io::stdout().flush();
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim(), "y" | "Y" | "yes")
}

fn render_list(names: &[&str]) -> String {
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
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

/// Everything `hekla project` was asked for beyond its paths. One struct because the
/// alternative is a free function with twelve positional arguments, half of them `bool`.
struct Ask<'a> {
    projector: Option<&'a str>,
    entity: Option<&'a str>,
    from: Option<u64>,
    upto: Option<u64>,
    max_events: Option<u64>,
    rows: usize,
    decrypt: bool,
    progress: bool,
    json: bool,
}

/// Fold an ad-hoc projector over the log and print its rows.
///
/// Exits zero whenever the projection ran, whatever it found: zero rows, a spent budget
/// and a bounded row list are all answers. `plan` states the same rule for a change, and
/// it holds harder here, where the rows *are* the result. Everything that can go wrong is
/// an error rather than a finding, so the report ends on `ok:` or `partial:` and never on
/// `failed:`.
fn project(file: &Path, dir: &Path, data_dir: Option<&Path>, ask: Ask<'_>) -> ExitCode {
    // Checked before the directory, because `[DIR]` defaulting to `.` makes the
    // transposed `hekla project . question.hk` easy to type, and a complaint about `.`
    // not being a `.hk` file is the one that names the real mistake.
    if !file.is_file() {
        eprintln!("error: `{}` is not a file", file.display());
        return ExitCode::FAILURE;
    }
    if file.extension().is_none_or(|ext| ext != "hk") {
        eprintln!(
            "error: `{}` is not a `.hk` file; the projector to fold goes in one",
            file.display()
        );
        return ExitCode::FAILURE;
    }
    // Same reason as `openapi` and `plan`: `load` succeeds vacuously on a path that is
    // not a project, and folding against a typo'd directory would report an empty answer
    // rather than a mistake.
    if !dir.is_dir() {
        eprintln!("error: `{}` is not a directory", dir.display());
        return ExitCode::FAILURE;
    }
    let source = match fs::read_to_string(file) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("error: reading `{}`: {err}", file.display());
            return ExitCode::FAILURE;
        }
    };

    let name = file.display().to_string();
    // Read the tree, then compile it once with this file in it. `load().with_scratch()`
    // would reach the same place having compiled the project twice and thrown the first
    // one away; the sources are what is expensive to gather, not what is expensive to
    // hold.
    let project = Sources::read(dir).compile(Some(Scratch {
        name: &name,
        source: &source,
    }));
    let findings = validate::findings(&project);
    for finding in &findings {
        eprintln!("{}", validate::render(finding));
    }
    let errors = validate::errors(&findings);
    if errors > 0 {
        eprintln!("refusing to project: the project has {errors} error(s)");
        return ExitCode::FAILURE;
    }

    // Resolved here rather than inside the projection, the way `plan` resolves its own:
    // a malformed master must fail the run that asked for it, and a projection that
    // seals no column never needs one at all.
    let master = match crypto::master_keys_from_env() {
        Ok(master) => master,
        Err(err) => {
            eprintln!("error: reading the master key: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    let data = runtime::resolve_data_dir(dir, data_dir);
    let request = projection::Request {
        projector: ask.projector,
        entity: ask.entity,
        from: ask.from,
        upto: ask.upto,
        max_events: ask.max_events,
        decrypt: ask.decrypt,
        rows: ask.rows,
        master,
    };
    // A follower, because this runs in its own process and the writer is elsewhere:
    // read-only descriptors, no lock, a prefix pinned at open. A served projection is in
    // the writer's own process and passes that writer's read handle instead.
    let store = match runtime::follow(&data) {
        Ok(Some(store)) => store,
        Ok(None) => {
            eprintln!(
                "error: no event log at {}, so there is nothing to project over",
                data.join("events").display()
            );
            return ExitCode::FAILURE;
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    let ticker = Progress::stderr(ask.progress);
    let result = projection::run(
        &project,
        &store,
        &data,
        &request,
        &mut |position, upto, matched| {
            ticker.tick(position.get(), upto.get(), matched);
        },
    );
    // Before either arm writes, so half a progress line can never sit under a result or
    // under a diagnostic.
    ticker.clear();

    match result {
        Ok(projection) => {
            if ask.json {
                match serde_json::to_string_pretty(&projection.json()) {
                    Ok(text) => println!("{text}"),
                    Err(err) => {
                        eprintln!("error: serializing the projection: {err}");
                        return ExitCode::FAILURE;
                    }
                }
            } else {
                println!("{projection}");
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// `hekla secrets`: what this machine can supply for the credentials the project
/// declares.
///
/// Never a value, and not because the printing is careful: it reads a
/// [`crate::secrets::Resolution`], which does not carry one. What it shows instead is
/// where each was looked for and a short fingerprint, which is what an operator comparing
/// staging against production actually needs.
///
/// Exits non-zero when a required credential is unset, unlike `plan`, which exits zero
/// whatever it finds. The difference is what each is for: a change is `plan`'s expected
/// result, and an unset credential is this command's failure condition.
fn secrets(dir: &Path) -> ExitCode {
    // The same guard `plan` and `openapi` apply, for the same reason: `load` succeeds
    // vacuously on a path that is not a project, and "0 credentials, all fine" for a
    // typo'd directory is the answer this command must never give.
    if !dir.is_dir() {
        eprintln!("error: `{}` is not a directory", dir.display());
        return ExitCode::FAILURE;
    }
    let project = LoadedProject::load(dir);
    let findings = validate::findings(&project);
    for finding in &findings {
        eprintln!("{}", validate::render(finding));
    }
    let errors = validate::errors(&findings);
    if errors > 0 {
        eprintln!("refusing to report: the project has {errors} error(s)");
        return ExitCode::FAILURE;
    }
    let (_, report) = crate::secrets::resolve(&project.program, &project.config, &project.root);
    if report.is_empty() {
        println!("this project declares no deployment credentials");
        return ExitCode::SUCCESS;
    }
    let width = report
        .iter()
        .map(|one| one.name.len())
        .max()
        .unwrap_or_default();
    for one in &report {
        let state = match (&one.fingerprint, one.optional) {
            (Some(fingerprint), _) => format!("set      {fingerprint}"),
            (None, true) => "unset    (optional)".to_owned(),
            (None, false) => "MISSING".to_owned(),
        };
        // A source that is there and unreadable says so. "not set" would send an
        // operator looking for a file that is right in front of them.
        let source = match &one.error {
            Some(why) => format!("{} ({why})", one.source),
            None => one.source.clone(),
        };
        println!(
            "  {:<width$}  {state:<20}  {source}",
            one.name,
            width = width
        );
    }
    let missing = report.iter().filter(|one| one.missing()).count();
    if missing == 0 {
        // "all set" would be a lie when an optional is deliberately unset, and that is
        // the one line an operator skims, so it counts what it actually checked.
        let unset = report.iter().filter(|one| !one.resolved()).count();
        match unset {
            0 => println!("\nok: {} credential(s), all set", report.len()),
            _ => println!(
                "\nok: {} credential(s), every required one set ({unset} optional unset)",
                report.len()
            ),
        }
        ExitCode::SUCCESS
    } else {
        println!(
            "\nfailed: {missing} of {} credential(s) unset; serving would refuse to start",
            report.len()
        );
        ExitCode::FAILURE
    }
}

/// `hekla plan`: what deploying this project over a data directory would change.
///
/// Findings go to stderr like `openapi`'s, because `--json` puts a machine-readable
/// document on stdout. It does not initialise tracing: it opens no runtime, so there is
/// nothing to trace.
///
/// Exits zero whenever the plan was computed, whether or not it is empty. `verify`
/// exits non-zero on a violation because a violation is a fault; a *change* is the
/// expected result of running `plan` at all, and a command that fails when it succeeds
/// is no use in a pipeline. A gate reads `--json`.
fn plan(
    dir: &Path,
    data_dir: Option<&Path>,
    json: bool,
    replay: bool,
    replay_limit: u32,
) -> ExitCode {
    // Same reason as `openapi`: `load` succeeds vacuously on a path that is not a
    // project, and reporting "nothing would change" for a typo'd directory is the one
    // answer this command must never give.
    if !dir.is_dir() {
        eprintln!("error: `{}` is not a directory", dir.display());
        return ExitCode::FAILURE;
    }
    let project = LoadedProject::load(dir);
    let findings = validate::findings(&project);
    for finding in &findings {
        eprintln!("{}", validate::render(finding));
    }
    let errors = validate::errors(&findings);
    if errors > 0 {
        eprintln!("refusing to plan: the project has {errors} error(s)");
        return ExitCode::FAILURE;
    }
    // Read only when a replay will use it. Without `--replay` this command opens no log
    // and reads no key material, and asking for a master it will not use would make a
    // malformed one fail a run that never needed it.
    let replay = if replay {
        let master = match crypto::master_keys_from_env() {
            Ok(master) => master,
            Err(err) => {
                eprintln!("error: reading the master key: {err:#}");
                return ExitCode::FAILURE;
            }
        };
        Replay::On {
            master,
            limit: replay_limit as usize,
        }
    } else {
        Replay::Off
    };
    let data = runtime::resolve_data_dir(dir, data_dir);
    match crate::plan::compute_with(&project, &data, replay) {
        Ok(plan) => {
            if json {
                match serde_json::to_string_pretty(&plan.json()) {
                    Ok(text) => println!("{text}"),
                    Err(err) => {
                        eprintln!("error: serializing the plan: {err}");
                        return ExitCode::FAILURE;
                    }
                }
            } else {
                println!("{plan}");
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// `hekla adopt`: put every subject key under the parent its subject declares.
///
/// The same pass `hekla serve` runs at boot, offered separately so the work can be done
/// before a deploy rather than during one. It reads the log through a follower and takes
/// no directory lock, so pointing it at the new project and a live data directory is the
/// intended use: the server still serving the old declaration keeps running, and the boot
/// that follows finds nothing left to do.
///
/// No confirmation, because it destroys nothing: the wrapping moves and the secret does
/// not, so every value stays exactly as readable as it was.
fn adopt(dir: &Path, data_dir: Option<&Path>, progress: bool) -> ExitCode {
    if !dir.is_dir() {
        eprintln!("error: `{}` is not a directory", dir.display());
        return ExitCode::FAILURE;
    }
    let project = LoadedProject::load(dir);
    let findings = validate::findings(&project);
    for finding in &findings {
        eprintln!("{}", validate::render(finding));
    }
    let errors = validate::errors(&findings);
    if errors > 0 {
        eprintln!("refusing to adopt: the project has {errors} error(s)");
        return ExitCode::FAILURE;
    }
    // Both answers before the master key is asked for, because neither needs one. A deploy
    // script that runs this over every project would otherwise fail on the flat ones, and
    // on a directory nothing has served yet, for want of a key there is nothing to use.
    // Said apart from each other too: "no hierarchy" and "already in order" look identical
    // from the key store and are different facts about the project.
    if crate::schema::Subjects::of(&project.program)
        .parented()
        .is_empty()
    {
        println!("no subject declares a parent, so there is nothing to adopt");
        return ExitCode::SUCCESS;
    }
    let data = runtime::resolve_data_dir(dir, data_dir);
    if !data.join("hekla.db").exists() {
        println!(
            "nothing is deployed at {}, so there is nothing to adopt",
            data.display()
        );
        return ExitCode::SUCCESS;
    }
    let master = match crypto::master_keys_from_env() {
        Ok(Some(master)) => master,
        Ok(None) => {
            eprintln!("error: HEKLA_MASTER_KEY must be set to adopt");
            return ExitCode::FAILURE;
        }
        Err(err) => {
            eprintln!("error: reading the master key: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    // Before opening anything. `Runtime::open_following` goes through `OpDb::open`, which
    // *migrates*, and this command's whole point is to be pointed at a directory a server
    // is still serving from: silently rewriting that server's schema under it is the one
    // thing it must not do. `plan` and `project` refuse the same way, for the same reason.
    match opdb::recorded_schema_version(&data.join("hekla.db")) {
        Ok(version) if version != opdb::SCHEMA_VERSION => {
            eprintln!(
                "error: the data directory is at schema version {version} and this build expects {}; run `hekla serve` against it once to migrate, or adopt with the build that wrote it",
                opdb::SCHEMA_VERSION
            );
            return ExitCode::FAILURE;
        }
        Ok(_) => {}
        Err(err) => {
            eprintln!("error: reading the operational database: {err:#}");
            return ExitCode::FAILURE;
        }
    }
    let runtime = match runtime::Runtime::open_following(&project, &data, Some(master)) {
        Ok(Some(runtime)) => runtime,
        // No log to fold, which is only good news if there is also nothing waiting. A
        // directory whose `events/` was moved or restored separately still holds its key
        // rows, and reporting success over those sent a deploy gate green on a store the
        // boot then refuses. The parent lives in the events, so without them there is
        // nothing that can place these and saying so is the whole of the help available.
        Ok(None) => {
            // Asked, not assumed. Both halves of this used `.unwrap_or(0)`, which turns
            // "I could not find out" into "nothing is waiting": a directory restored
            // without `events/` and with a corrupt `hekla.db` reported success, which is
            // the precise shape this guard was added to stop.
            let subjects = crate::schema::Subjects::of(&project.program);
            let waiting = match OpDb::open(&data.join("hekla.db"))
                .and_then(|db| db.count_roots_of(&subjects.parented()))
            {
                Ok(waiting) => waiting,
                Err(err) => {
                    eprintln!("error: reading the operational database: {err:#}");
                    return ExitCode::FAILURE;
                }
            };
            if waiting > 0 {
                eprintln!(
                    "error: {waiting} subject key(s) are waiting to be adopted, and there is no event log at {} to learn their parents from",
                    data.join("events").display()
                );
                return ExitCode::FAILURE;
            }
            println!("no event log at {}: nothing to adopt", data.display());
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    // The keystore is always there: a missing master was refused above and
    // `open_following` builds one from whatever it is given. Named rather than unwrapped
    // so a future change to either does not turn into a panic here.
    let Some(keystore) = runtime.keystore() else {
        eprintln!("error: no master key, so there are no keys to adopt");
        return ExitCode::FAILURE;
    };
    // The same two refusals `serve` makes, and for the same reasons. Without them this
    // command fails *differently* from the boot it is meant to run ahead of: a missing
    // master surfaces halfway through, after rows have already moved, and a declaration
    // that cannot read the log surfaces as a raw decode error at some position instead of
    // naming the field and the `@absent` that answers it.
    if let Err(err) = keystore.verify_masters_present() {
        eprintln!("error: {err:#}");
        return ExitCode::FAILURE;
    }
    let recorded = match runtime.opdb().lock() {
        Ok(db) => db.recorded_entries(),
        Err(poisoned) => poisoned.into_inner().recorded_entries(),
    };
    match recorded.and_then(|recorded| {
        crate::heklang_host::history_faults(runtime.program(), &recorded, runtime.store())
    }) {
        Ok(unreadable) => {
            if let Some(refusal) = crate::heklang_host::unreadable_refusal(
                &unreadable,
                "adopt",
                "nothing has been moved and correcting the declaration is the whole of the repair",
            ) {
                eprintln!("error: {refusal}");
                return ExitCode::FAILURE;
            }
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            return ExitCode::FAILURE;
        }
    }
    let head = runtime.store().head().get();
    let ticker = Progress::stderr(progress);
    let done = crate::adopt::run(
        runtime.program(),
        runtime.events_map(),
        runtime.store(),
        keystore,
        &mut |position, resolved| ticker.tick(position, head, resolved),
    );
    ticker.clear();
    match done {
        Ok(done) => {
            for wrong in &done.disagreements {
                eprintln!("warning: {}", crate::adopt::disagreement_line(wrong));
            }
            let (adopted, scanned) = (done.adopted, done.scanned);
            if adopted == 0 && done.waiting == 0 {
                println!("every subject key is already under its declared parent");
            } else {
                println!(
                    "adopted {adopted} subject key(s) under their declared parent, reading {scanned} event(s)"
                );
            }
            // The two endings are not the same here, and treating them alike made this
            // command fail on the directory it exists for. A live deployment is still
            // serving the *old* declaration while this runs, so it keeps minting roots
            // this run will never catch: contention is the normal condition of a
            // pre-flight, not a fault. The boot refuses on it because nothing else should
            // be writing there; this says what is left and exits clean, so a deploy script
            // can run it without pretending the store is quiet.
            match crate::adopt::unfinished(&done) {
                // Two of the three are faults wherever they are found: a subject no event
                // accounts for, and a row the store could not move at all. Only the race
                // with a live writer is ordinary here, and lumping the others in with it
                // told an operator to go looking for a second process and exited clean.
                Some(fault @ crate::adopt::Unfinished::Unaccounted { .. })
                | Some(fault @ crate::adopt::Unfinished::Failed { .. }) => {
                    eprintln!("error: {fault}");
                    ExitCode::FAILURE
                }
                Some(contended) => {
                    eprintln!("warning: {contended}");
                    eprintln!(
                        "  they will be moved by the boot that deploys this project, which runs with nothing else writing"
                    );
                    ExitCode::SUCCESS
                }
                None => ExitCode::SUCCESS,
            }
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// `hekla verify`: the offline invariant sweep over a data directory.
///
/// Exits non-zero on any violation, so it drops straight into CI or a nightly job.
/// It reports what it checked even when clean, because a sweep that found nothing
/// because it covered nothing must not read like a passing one.
fn verify(dir: &Path, data_dir: Option<&Path>) -> ExitCode {
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
    // Before the project loads and long before a projector or effect thread starts: a
    // counter recorded with no recorder installed is dropped, so anything emitted ahead
    // of this would be invisible until the process happened to do it again.
    metrics::install();

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

/// Print every finding and return the (error, warning) counts.
fn report_findings(project: &LoadedProject) -> (usize, usize) {
    let findings = validate::findings(project);
    print_findings(&findings);
    let errors = validate::errors(&findings);
    (errors, findings.len() - errors)
}

/// Whether log lines carry ANSI colors: the operator has not opted out through
/// `--no-color` or `NO_COLOR`, and the destination is a terminal rather than a file or
/// a pipe.
///
/// `tracing_subscriber` honours `NO_COLOR` in its own default but never looks at the
/// destination, so the whole decision is taken here instead. The redirect is the case
/// that matters: a log captured by systemd or a CI job must come out clean without the
/// operator having to know a flag exists.
fn use_ansi(no_color: bool, is_terminal: bool, no_color_env: Option<&str>) -> bool {
    !no_color && is_terminal && no_color_env.is_none_or(str::is_empty)
}

fn init_tracing(no_color: bool) {
    let no_color_env = env::var("NO_COLOR").ok();
    let ansi = use_ansi(
        no_color,
        io::stdout().is_terminal(),
        no_color_env.as_deref(),
    );
    let filter = match env::var("RUST_LOG") {
        Ok(directives) => EnvFilter::try_new(&directives).unwrap_or_else(|err| {
            // Falling back in silence tells an operator debugging with RUST_LOG that the
            // level they asked for revealed nothing, when it was never applied.
            eprintln!("warning: ignoring RUST_LOG=\"{directives}\": {err}");
            EnvFilter::new(DEFAULT_LOG_FILTER)
        }),
        Err(_) => EnvFilter::new(DEFAULT_LOG_FILTER),
    };
    if let Err(err) = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(ansi)
        .try_init()
    {
        eprintln!("warning: keeping the log subscriber already installed: {err}");
    }
}

fn print_findings(findings: &[Finding]) {
    for finding in findings {
        println!("{}", validate::render(finding));
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

    /// The redirect is the case the flag exists for, and it must not need the flag.
    #[test]
    fn colors_are_off_unless_the_destination_is_a_terminal_nobody_opted_out_of() {
        assert!(use_ansi(false, true, None));
        assert!(!use_ansi(true, true, None));
        assert!(!use_ansi(false, false, None));
        // An empty `NO_COLOR` is not an opt-out, the same reading tracing-subscriber
        // and the no-color-org convention take.
        assert!(use_ansi(false, true, Some("")));
        assert!(!use_ansi(false, true, Some("1")));
    }
}
