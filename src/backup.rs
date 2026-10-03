//! Copying a data directory a server is still writing.
//!
//! The log can be taken from under the writer with no coordination at all. tephra mutates
//! nothing and deletes nothing, and a batch counts only if every record in it validates by
//! CRC *and* the run ends in a commit marker, so a file-level copy is always a committed
//! prefix and a torn tail rolls back when it is opened. The operational database is one
//! `VACUUM INTO` away from the same thing. What an operator reaching for `cp`, `rsync` or
//! Borg cannot know is the order, and the order on its own is not enough:
//!
//! - **The key store must be at least as new as the log.** A missing subject key is
//!   indistinguishable from an erasure (see [`crate::crypto`]), so a key store copied before
//!   the log it is paired with makes live data read as though those subjects had been
//!   forgotten, and nothing reports it. That is why the log is copied first here.
//! - **The effect tables must be no newer than the log.** A restored log continues at
//!   `head + 1`, and [`OpDb::begin_invocation`](crate::opdb::OpDb::begin_invocation) keys on
//!   `(effect, position)` alone, so a terminal row left above the head makes a *different*
//!   event report `AlreadyTerminal` and skip its effect. Nothing reports that either.
//!
//! Both live in `hekla.db`, so no ordering of two snapshots satisfies both constraints, and
//! the second is repaired instead of ordered for: [`clamp_to_log`] lowers every position the
//! copy records to the head of the log it was paired with.
//!
//! Read models are not copied. A read model cannot be rewound, because its rows already
//! reflect events above its checkpoint, and it is rebuildable from the log by construction,
//! so leaving it out is the only correct choice rather than a saving. A restore builds them
//! from scratch, which `projector::reconcile_from` reads as a fresh model rather than a stale
//! one however `auto_rebuild` is set.
//!
//! **Nothing here opens a database through [`OpDb`](crate::opdb::OpDb).** `OpDb::open`
//! migrates, and a backup must change neither the source, which a server has open, nor the
//! copy it is writing: the copy leaves here at the schema version that wrote it. The queries
//! this needs are written out instead. What happens to the copy afterwards is not this
//! module's: anything that opens it, `hekla verify` included, migrates it and recovers its
//! active segment, so a target that has been checked is no longer byte-identical to the one
//! that was written.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use anyhow::Context;
use rusqlite::{Connection, OpenFlags, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::lock::DataDirLock;
use crate::opdb;
use crate::progress::{self, Progress};
use crate::runtime;

/// The manifest, written last.
///
/// It doubles as what marks a directory as a backup target: a non-empty target without one
/// is refused rather than written into, because it might be somebody's data directory.
pub const MANIFEST: &str = "hekla-backup.json";

/// The suffix a file is copied under before it is renamed into place, so a run that dies
/// half way through leaves the previous copy intact rather than a plausible-looking file of
/// the right length holding half the bytes. tephra ignores any name it cannot parse as a
/// segment, so a leftover is inert as well as swept.
const INCOMING: &str = ".incoming";

/// tephra's single-writer lock file, which is not copied.
///
/// The lock itself is a POSIX record lock held by a live process, so the file carries nothing
/// but the holder's pid, for the sake of a contended error message. Copying it would put
/// another process's pid in the backup, and a restore creates its own.
const TEPHRA_LOCK: &str = "LOCK";

/// How much is read or written at a time, by both the backwards extent scan and the copy.
const CHUNK: usize = 1 << 20;

/// What a backup moved, and what the clamp had to discard to make it consistent.
#[derive(Debug, Serialize)]
pub struct Report {
    /// The build that wrote this backup.
    pub hekla: &'static str,
    /// Whether this is a finished backup.
    ///
    /// Written `false` before the log is touched and `true` only once the state has been
    /// installed beside it, because the window between those two is the one state this
    /// directory must never be mistaken for a backup in: the log has grown and the key store
    /// has not, which is the silent-erasure shape the whole module exists to avoid. A target
    /// left this way is repaired by the next run, and until then it says so.
    pub complete: bool,
    pub taken_at: String,
    /// Canonical, so that `./data` and `/var/lib/hekla` are recognised as one directory and a
    /// genuinely different one is recognised as different.
    pub source: PathBuf,
    pub target: PathBuf,
    /// The last position in the copied log. Everything the state records is at or below it.
    pub log_head: u64,
    /// The schema the copy holds, which is the one the source held: a backup migrates
    /// nothing. A restore migrates on boot.
    pub schema_version: i64,
    /// Every master the copied subject keys are wrapped under.
    ///
    /// The master key itself is not in a data directory, so this is the one thing standing
    /// between a backup and being inert: without one of these, every sealed field in it
    /// reads as erased. Two is legitimate, mid-rotation.
    pub master_key_ids: Vec<String>,
    pub keys: u64,
    pub files_copied: usize,
    pub files_reused: usize,
    pub bytes_copied: u64,
    pub discarded: Discarded,
    /// The read models a restore will rebuild, by name. Read from the source directory
    /// rather than from a project, so it reports what is there rather than what something
    /// declares.
    pub projectors_skipped: Vec<String>,
}

/// What [`clamp_to_log`] took out of the copy.
///
/// Reported rather than quietly applied: a clamp discards work the source has recorded as
/// done, and an operator reading a summary that did not mention it would have no way to
/// know the restore will run those positions again.
#[derive(Debug, Default, Serialize)]
pub struct Discarded {
    pub invocations: usize,
    pub journal_rows: usize,
    pub lane_rows: usize,
    pub quarantines: usize,
    pub cursors_lowered: usize,
    pub boundaries_lowered: usize,
    /// Rows from a table [`opdb::POSITIONED_TABLES`] names and this struct has no field for,
    /// which a later migration can produce. Counted rather than dropped, so a clamp that did
    /// something is never reported as a clamp that did nothing.
    pub other_rows: usize,
}

impl Discarded {
    pub fn total(&self) -> usize {
        self.invocations
            + self.journal_rows
            + self.lane_rows
            + self.quarantines
            + self.cursors_lowered
            + self.boundaries_lowered
            + self.other_rows
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "copied {} file(s) ({}) and reused {}, to {}",
            self.files_copied,
            progress::size(self.bytes_copied),
            self.files_reused,
            self.target.display(),
        )?;
        writeln!(
            f,
            "log head {}, schema v{}",
            progress::thousands(self.log_head),
            self.schema_version,
        )?;
        // A v10 child is wrapped under its parent rather than under a master, so a store whose
        // roots have all been erased has keys and no masters. Saying "wrapped under master "
        // with nothing after it would be the one line here that has to be right.
        match (self.keys, self.master_key_ids.as_slice()) {
            (0, _) => writeln!(f, "no subject keys, so no master key is needed to read it")?,
            (keys, []) => writeln!(
                f,
                "{} subject key(s) and no master among them: every root they descend from has \
                 been erased, so they unwrap nothing",
                progress::thousands(keys),
            )?,
            (keys, masters) => writeln!(
                f,
                "{} subject key(s), wrapped under master {}: without it every sealed field \
                 reads as erased",
                progress::thousands(keys),
                masters.join(", "),
            )?,
        }
        // Enumerated only when there was something to enumerate. A backup of a settled
        // directory is the common case and six zeros say nothing; a backup that discarded
        // work must say exactly what.
        if self.discarded.total() == 0 {
            writeln!(f, "clamped to the log head: nothing recorded sat above it")?;
        } else {
            write!(
                f,
                "clamped to the log head: discarded {} invocation(s), {} journal row(s), \
                 {} lane row(s), {} quarantine(s), and lowered {} cursor(s), \
                 {} live boundary(ies)",
                self.discarded.invocations,
                self.discarded.journal_rows,
                self.discarded.lane_rows,
                self.discarded.quarantines,
                self.discarded.cursors_lowered,
                self.discarded.boundaries_lowered,
            )?;
            // Unreachable until a migration adds a positioned table this build has no field
            // for, and said rather than dropped when it does: a clamp that discarded rows must
            // not report a line that adds up to fewer.
            if self.discarded.other_rows > 0 {
                write!(f, ", and {} further row(s)", self.discarded.other_rows)?;
            }
            writeln!(f)?;
        }
        if !self.projectors_skipped.is_empty() {
            writeln!(
                f,
                "a restore rebuilds {} read model(s): {}",
                self.projectors_skipped.len(),
                self.projectors_skipped.join(", "),
            )?;
        }
        write!(
            f,
            "check it with `hekla verify <project> --data-dir {}`",
            self.target.display(),
        )
    }
}

/// The data directory `source` names: the directory itself, or the one a project holds at
/// `<source>/data`.
///
/// Every other subcommand takes a project and an optional `--data-dir`, because it needs the
/// code and the data together and only the project can say where the code is. A backup needs no
/// project at all, so that indirection here would be a flag with nothing behind it: one
/// argument names the deployment, spelled whichever way the operator has it to hand, and
/// `hekla backup . /backups` and `hekla backup /var/lib/hekla /backups` both mean something.
///
/// `hekla.toml` cannot move the data directory, so `<source>/data` is the whole of the
/// convention rather than a guess at one.
pub fn resolve_source(source: &Path) -> anyhow::Result<PathBuf> {
    if is_data_dir(source) {
        return Ok(source.to_path_buf());
    }
    let nested = source.join("data");
    if is_data_dir(&nested) {
        return Ok(nested);
    }
    // Both named, and what one looks like, rather than a guess at which was meant. There is no
    // sound way to say which half is missing: a project's `events/` holds modules rather than
    // segments, so "there is no operational database beside your `events/`" is what a project
    // directory would otherwise be told, naming a path nobody typed.
    anyhow::bail!(
        "no data directory to back up: neither {source} nor {nested} holds both an `events/` \
         directory and a `hekla.db`",
        source = source.display(),
        nested = nested.display(),
    )
}

/// Copy `source` to `target`, consistently, without disturbing whatever has `source` open.
///
/// `source` is a data directory or a project holding one; see [`resolve_source`].
///
/// Repeat runs update `target` in place: everything under `events/` is immutable except the
/// one segment being appended to, so a second run copies that and whatever is new, and the
/// state is replaced wholesale. Replacing it is what keeps an erasure propagating, since the
/// target always carries the newest key store and there is no older copy in it to resurrect a
/// key from.
pub fn run(source: &Path, target: &Path, progress: &Progress) -> anyhow::Result<Report> {
    let source = &resolve_source(source)?;
    let state = source.join("hekla.db");

    // Before anything is copied. A directory written by a newer build may record positions
    // in tables this one does not know to clamp, and a clamp that misses a table leaves
    // exactly the silent skip the clamp exists to prevent. An *older* version is fine and
    // deliberately allowed: a backup is the thing you want before an upgrade.
    let schema_version = opdb::recorded_schema_version(&state)?;
    anyhow::ensure!(
        schema_version <= opdb::SCHEMA_VERSION,
        "the data directory is at schema version {schema_version} and this build knows up to \
         {}; back it up with the build that wrote it, which is the one that knows every \
         position it records",
        opdb::SCHEMA_VERSION
    );

    // Asked of the filesystem, and only about the file's existence: that is what says this
    // directory is a backup rather than somebody's data. What the manifest *says* is read
    // further down, once the lock is held, and is a refusal of its own if it cannot be.
    anyhow::ensure!(
        target.join(MANIFEST).exists() || is_empty(target)?,
        "{} is not empty and holds no {MANIFEST}; refusing to write into it, in case it is a \
         data directory",
        target.display()
    );

    fs::create_dir_all(target).with_context(|| format!("creating {}", target.display()))?;
    // Not the source's lock, which a running server holds and this must never contend for.
    // This one makes two backups into one target serialise, and refuses to write into a
    // directory something is serving from.
    let _lock = DataDirLock::acquire(target).with_context(|| {
        format!(
            "refusing to back up into {}, which is in use",
            target.display()
        )
    })?;

    // Canonical, so the two checks below compare directories rather than spellings of them.
    let canonical =
        fs::canonicalize(source).with_context(|| format!("resolving {}", source.display()))?;
    // Read before the copy, so it is a head the copy is guaranteed to reach: committed bytes
    // never change, so everything at or below this is already final on disk.
    let before = log_head(source)?;
    let previous = read_manifest(target)?;
    if let Some(previous) = &previous {
        // A target belongs to one data directory. Nothing downstream would notice otherwise:
        // the reuse check matches by name, so a second source's segments would be interleaved
        // with the first's into one log whose records all pass their CRC.
        if let Some(recorded) = &previous.source
            && recorded != &canonical
        {
            anyhow::bail!(
                "{target} holds a backup of {recorded}, not of {source}; back up to a \
                 different target, or remove this one if that directory is gone",
                target = target.display(),
                recorded = recorded.display(),
                source = canonical.display(),
            );
        }
    }
    // The target's *own* head, not the one its manifest records. They differ exactly when
    // something other than a backup has written to it, which the docs sanction: serving from
    // a target is how a restore works. Appending to it locally and then backing up over it
    // would otherwise splice two timelines, since the copy overwrites the segments it shares
    // by name and the head is read back off whatever tail was left.
    let held = log_head(target)?;
    anyhow::ensure!(
        held <= before,
        "{target} holds a log at head {held}, which is ahead of the {before} at {source}: an \
         append-only log cannot fall behind its own backup, so this is either a different \
         deployment or a copy something has served from and appended to",
        target = target.display(),
        source = canonical.display(),
    );

    // Whatever a dead run left staged, which `copy_log` cannot reach: the state snapshot and
    // the manifest are staged at the target's root, and a full-size `hekla.db.incoming` is the
    // largest thing a failed run can leave behind.
    sweep_incoming(target)?;

    // Claim the target before the log is touched, and say the claim is unfinished.
    //
    // Both halves earn their place. Claiming it means a run that dies part way through leaves
    // a directory the next run will still write into, rather than a non-empty one with no
    // manifest that every later run refuses. Saying it is unfinished means the window between
    // the log copy and the state landing beside it is legible: in it, the target holds a log
    // that has grown past the key store next to it, which is indistinguishable from an
    // erasure of everything in between and must not read as a backup.
    write_manifest(target, &claim(&canonical, target, &previous))?;

    let copied = copy_log(source, target, progress)?;
    let incoming = target.join(format!("hekla.db{INCOMING}"));
    progress.paint(|_| "  snapshotting the key store and the journal".to_owned());
    snapshot_state(source, &incoming)?;

    let head = log_head(target)?;
    anyhow::ensure!(
        head >= before,
        "the copied log stops at {head}, below the {before} the source had already committed \
         when the copy started; the copy is incomplete"
    );
    // Read models are not part of a backup, and a target that has been booted or verified has
    // built its own. Leaving them would make the next restore come up against models built
    // from an older definition, which `auto_rebuild = false` reports as stale rather than
    // rebuilding, and would make this run's own summary wrong about what a restore will do.
    let models = target.join("projectors");
    if models.exists() {
        fs::remove_dir_all(&models).with_context(|| format!("removing {}", models.display()))?;
    }

    let discarded = clamp_to_log(&incoming, head)?;
    let keys = count_keys(&incoming)?;
    let master_key_ids = master_key_ids(&incoming)?;
    install(&incoming, target)?;

    let report = Report {
        hekla: env!("CARGO_PKG_VERSION"),
        complete: true,
        taken_at: runtime::now_rfc3339(),
        source: canonical,
        target: target.to_path_buf(),
        log_head: head,
        schema_version,
        master_key_ids,
        keys,
        files_copied: copied.copied,
        files_reused: copied.reused,
        bytes_copied: copied.bytes,
        discarded,
        projectors_skipped: read_models(source)?,
    };
    write_manifest(target, &report)?;
    Ok(report)
}

// --- the log ---------------------------------------------------------------

/// What a log copy moved, and what it did not have to.
#[derive(Debug, Default)]
pub struct Copied {
    pub copied: usize,
    pub reused: usize,
    pub bytes: u64,
}

/// Copy `source/events` into `target/events`.
///
/// Safe against the writer by the format's own rules, and cheap on a second run by them too:
/// a sealed segment is never written again and a sealed segment's `.idx` is written once when
/// it seals, so the only file that can have changed is the one with the highest base position.
/// Correctness does not rest on that: an `.idx` that is stale or torn fails its body CRC and
/// is rebuilt from the log, which is why it needs no coordination either.
pub fn copy_log(source: &Path, target: &Path, progress: &Progress) -> anyhow::Result<Copied> {
    let from = source.join("events");
    let to = target.join("events");
    let files = list_files(&from)?;
    let active = active_segment(&files);
    sweep_incoming(&to)?;
    // Created here rather than in the loop below, which a source whose `events/` holds nothing
    // never enters: an empty one is a real state, since a boot that died between creating the
    // directory and initialising the log leaves exactly that.
    fs::create_dir_all(&to).with_context(|| format!("creating {}", to.display()))?;
    let held = list_files(&to)?;
    let present: HashSet<PathBuf> = files.iter().map(|(rel, _)| rel.clone()).collect();

    // Every extent up front, the source's and whatever the target already holds, because the
    // reuse decision needs both and the copy needs the first.
    //
    // The scan is backwards, which is the only safe direction (it can only over-estimate where
    // the data ends, and the recovery rule discards what sits above the last commit marker),
    // and it is cheap for a sealed segment, whose last chunk is data. The one it is not cheap
    // for is a freshly rolled segment holding a few events: those are 255 chunks of real
    // `fallocate`d zeros, read in full, once per run. That is the cost of not trusting a
    // length, and the alternative, stopping at the first all-zero chunk from the front, would
    // cut a record whose payload happened to hold a run of zeros.
    let mut planned = Vec::new();
    let mut reused = 0;
    for (rel, len) in files {
        let prefix = extent(&from.join(&rel), len)?;
        let dest = to.join(&rel);
        // **Length cannot say whether a copy is complete.** A segment is `fallocate`d to its
        // full size when it is created and keeps that size when it seals, so every segment
        // file in the target matches every segment file in the source by length. The extent
        // is what differs: a segment copied while it was still being appended to holds a
        // shorter prefix of the same bytes, and once it seals, a reuse check that looked only
        // at the length would keep that truncated copy for ever. The log it opened into would
        // be short by the difference, which is a backup quietly missing committed events.
        //
        // The active segment is copied regardless, rather than relying on the extents to
        // differ. A rolled-back partial batch can be overwritten by a different one of the
        // same length, so for the one file the writer is touching, equal extents do not mean
        // equal bytes.
        if Some(&rel) != active.as_ref()
            && dest.metadata().is_ok_and(|meta| meta.len() == len)
            && extent(&dest, len).is_ok_and(|held| held == prefix)
        {
            reused += 1;
            continue;
        }
        planned.push((rel, len, prefix));
    }
    let total: u64 = planned.iter().map(|(_, _, prefix)| prefix).sum();

    // Whatever the target holds that the source does not. A log only grows, so this is never
    // an old segment being retired: it is a file that came from somewhere else, and leaving it
    // beside the segments this run copies would hand tephra a chain built from two logs.
    for (rel, _) in &held {
        if !present.contains(rel) {
            let stale = to.join(rel);
            fs::remove_file(&stale).with_context(|| format!("removing {}", stale.display()))?;
        }
    }

    let mut bytes = 0;
    let mut files_copied = 0;
    let count = planned.len();
    for (rel, len, prefix) in planned {
        let dest = to.join(&rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut moved = 0;
        // Painted from inside the copy, not between files: the shape this exists for is one
        // segment of a few hundred megabytes, and a line drawn only when a file finished
        // would appear for the first time once there was nothing left to report.
        copy_prefix(&from.join(&rel), &dest, prefix, len, |chunk| {
            moved += chunk;
            let done = bytes + moved;
            progress
                .paint(|elapsed| progress::copied(done, total, files_copied + 1, count, elapsed));
        })?;
        bytes += prefix;
        files_copied += 1;
    }
    // The bytes are synced by each copy; these are the directory entries that point at them,
    // which `sync_all` on a file inside a directory does not make durable. Without this a
    // backup that reported success could come back from a power loss short a whole segment.
    sync_dir(&to)?;
    if to.join("index").is_dir() {
        sync_dir(&to.join("index"))?;
    }
    Ok(Copied {
        copied: files_copied,
        reused,
        bytes,
    })
}

/// The committed extent of one file, or an error naming it.
fn extent(path: &Path, len: u64) -> anyhow::Result<u64> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    committed_extent(&file, len).with_context(|| format!("measuring {}", path.display()))
}

/// The head of the log under `dir`, or `0` when there is no log there.
///
/// Opened as a follower: read-only descriptors, no lock, nothing created and nothing deleted,
/// so this runs against a directory a server is writing as happily as against a copy.
pub fn log_head(dir: &Path) -> anyhow::Result<u64> {
    Ok(match runtime::follow(dir)? {
        Some(store) => store.head().get(),
        None => 0,
    })
}

/// Every file under `dir`, relative to it, with its length.
fn list_files(dir: &Path) -> anyhow::Result<Vec<(PathBuf, u64)>> {
    fn walk(dir: &Path, prefix: &Path, into: &mut Vec<(PathBuf, u64)>) -> anyhow::Result<()> {
        let entries = fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
            let rel = prefix.join(entry.file_name());
            // `fs::metadata` rather than `DirEntry::metadata`, which does not follow a
            // symlink: a symlinked segment would report the length of the link itself, and
            // the copy, which opens the path and so does follow it, would be cut to that.
            let meta = fs::metadata(entry.path())
                .with_context(|| format!("reading {}", entry.path().display()))?;
            if meta.is_dir() {
                walk(&entry.path(), &rel, into)?;
            } else if entry.file_name() != TEPHRA_LOCK && !staging(&rel) {
                into.push((rel, meta.len()));
            }
        }
        Ok(())
    }
    let mut found = Vec::new();
    walk(dir, Path::new(""), &mut found)?;
    found.sort();
    Ok(found)
}

/// The segment being appended to: the highest base position, which because the names are
/// zero-padded to a fixed width is also the highest name.
fn active_segment(files: &[(PathBuf, u64)]) -> Option<PathBuf> {
    files
        .iter()
        .map(|(rel, _)| rel)
        .filter(|rel| rel.extension().is_some_and(|ext| ext == "log"))
        .max()
        .cloned()
}

/// Delete whatever a previous run left part-copied, so a sealed segment that died half way
/// through does not keep a quarter of a gigabyte of the target for ever.
fn sweep_incoming(dir: &Path) -> anyhow::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            sweep_incoming(&path)?;
        } else if staging(&path) {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }
    Ok(())
}

/// Whether `path` is something a run staged rather than something it produced.
///
/// *Contains* the suffix rather than ends with it, because SQLite names its sidecars after
/// the database: a clamp killed mid-transaction leaves `hekla.db.incoming-journal`, and a
/// hot journal left beside the *next* run's freshly vacuumed snapshot of the same name is a
/// rollback onto a database it was never written for.
fn staging(path: &Path) -> bool {
    path.to_string_lossy().contains(INCOMING)
}

/// An upper bound on the committed extent: the offset just past the last non-zero byte.
///
/// A segment is `fallocate`d to its full size at creation and never extended, so copying one
/// whole moves 256 MiB however few events are in it. The cut can never land below the real
/// extent, because a committed record is not all zeros, and whatever sits above the last
/// commit marker is discarded when the copy is opened. Scanned backwards, so a full segment
/// is answered by its last chunk.
fn committed_extent(file: &File, len: u64) -> io::Result<u64> {
    let mut buf = vec![0u8; CHUNK];
    let mut end = len;
    while end > 0 {
        let start = end.saturating_sub(CHUNK as u64);
        let span = (end - start) as usize;
        file.read_exact_at(&mut buf[..span], start)?;
        if let Some(index) = buf[..span].iter().rposition(|byte| *byte != 0) {
            return Ok(start + index as u64 + 1);
        }
        end = start;
    }
    Ok(0)
}

/// Copy the first `prefix` bytes of `from` to `to` and give the result the full `len`, so the
/// copy reads back byte for byte as the source does: data, then the zero tail.
///
/// The tail is a hole where the source's was `fallocate`d, which is what makes a backup of a
/// mostly-empty 256 MiB segment cost kilobytes. What it does not carry over is the source's
/// space *reservation*, and nothing re-takes it on a restore, so a deployment served from a
/// copy can meet ENOSPC inside a segment the original had room for. Said in the docs rather
/// than fixed here: there is no way to `fallocate` through `std`, and taking a dependency for
/// it would buy a guarantee the copy is not the place to make.
///
/// Through a temporary name, because the reuse check above trusts a name that is there to be
/// complete.
fn copy_prefix(
    from: &Path,
    to: &Path,
    prefix: u64,
    len: u64,
    mut wrote: impl FnMut(u64),
) -> anyhow::Result<()> {
    let source = File::open(from).with_context(|| format!("opening {}", from.display()))?;
    let incoming = to.with_file_name(format!(
        "{}{INCOMING}",
        to.file_name().unwrap_or_default().to_string_lossy()
    ));
    let mut write = || -> io::Result<()> {
        let mut dest = File::create(&incoming)?;
        let mut buf = vec![0u8; CHUNK];
        let mut done = 0;
        while done < prefix {
            let span = (prefix - done).min(CHUNK as u64) as usize;
            source.read_exact_at(&mut buf[..span], done)?;
            dest.write_all(&buf[..span])?;
            done += span as u64;
            wrote(span as u64);
        }
        dest.set_len(len)?;
        dest.sync_all()
    };
    write().with_context(|| format!("writing {}", incoming.display()))?;
    fs::rename(&incoming, to)
        .with_context(|| format!("renaming {} into place", incoming.display()))?;
    Ok(())
}

/// Make a directory's own entries durable, which `sync_all` on a file inside it does not.
fn sync_dir(dir: &Path) -> anyhow::Result<()> {
    File::open(dir)
        .and_then(|handle| handle.sync_all())
        .with_context(|| format!("syncing {}", dir.display()))
}

// --- the state -------------------------------------------------------------

/// Snapshot `source`'s operational database into `into`.
///
/// `VACUUM INTO` through a read-only connection, which is a consistent snapshot of one
/// committed moment even while the server commits into the source, and lands as a single file
/// with no `-wal` beside it. Read-only is load-bearing rather than tidy: a read-write
/// connection takes `SQLITE_BUSY` against a committing writer unless it also sets a busy
/// timeout, and it has no business being able to write here at all.
///
/// Public, and separate from [`run`], so the test that takes the halves in the wrong order can
/// show what that costs.
pub fn snapshot_state(source: &Path, into: &Path) -> anyhow::Result<()> {
    // The sidecars go with it. A clamp killed mid-transaction leaves a `-journal` named after
    // this file, and SQLite would replay that onto the fresh snapshot this is about to put
    // under the same name, rolling back pages from a database it knows nothing about.
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let stale = PathBuf::from(format!("{}{suffix}", into.display()));
        if stale.exists() {
            fs::remove_file(&stale).with_context(|| format!("removing {}", stale.display()))?;
        }
    }
    let path = source.join("hekla.db");
    let live = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {} read-only", path.display()))?;
    let target = into
        .to_str()
        .with_context(|| format!("{} is not valid UTF-8", into.display()))?;
    live.execute("VACUUM INTO ?1", params![target])
        .with_context(|| format!("snapshotting {} into {target}", path.display()))?;
    Ok(())
}

/// Lower every position the operational database at `db` records to `head`.
///
/// The half of a backup that no ordering can buy. A restored log continues at `head + 1`, so
/// a row above the head is a claim about an event that does not exist yet and will be a
/// claim about a *different* event once one is appended. Discarding them is not a loss: they
/// describe positions the copied log does not contain, so nothing re-runs because of this.
///
/// [`OpDb::rewind_effect`](crate::opdb::OpDb::rewind_effect) is the same work for one effect,
/// and the thing to read to understand why it is needed, but it *sets* the cursor where this
/// may only lower it: a cursor that is already behind the head is a resume point, not
/// something to discard.
///
/// Every statement is guarded on the table existing, so a copy from an older schema (which
/// has no `effect_lane`, or no `ON DELETE CASCADE` on the journal) clamps as far as it goes
/// rather than failing.
pub fn clamp_to_log(db: &Path, head: u64) -> anyhow::Result<Discarded> {
    let mut conn = Connection::open(db).with_context(|| format!("opening {}", db.display()))?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .context("enabling foreign keys")?;
    let tx = conn.transaction().context("beginning a clamp")?;

    // Driven off `opdb`'s own list, in its order, so that a migration adding a table that
    // carries a position has one place to be declared rather than two to be kept in step.
    let mut discarded = Discarded::default();
    for table in opdb::POSITIONED_TABLES {
        let rows = delete_above(&tx, table, head)?;
        match *table {
            "effect_journal" => discarded.journal_rows = rows,
            "effect_invocation" => discarded.invocations = rows,
            "effect_lane" => discarded.lane_rows = rows,
            "effect_quarantine" => discarded.quarantines = rows,
            // Counted into the total so a clamp of a table this build does not have a field
            // for still reports that it happened.
            _ => discarded.other_rows += rows,
        }
    }
    for (table, column) in opdb::WATERMARK_COLUMNS {
        let rows = lower(&tx, table, column, head)?;
        match *column {
            "watermark" => discarded.cursors_lowered = rows,
            "live_boundary" => discarded.boundaries_lowered = rows,
            _ => discarded.other_rows += rows,
        }
    }
    tx.commit().context("committing a clamp")?;
    Ok(discarded)
}

fn has_table(tx: &Transaction<'_>, table: &str) -> anyhow::Result<bool> {
    let count: i64 = tx
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![table],
            |row| row.get(0),
        )
        .with_context(|| format!("looking for the `{table}` table"))?;
    Ok(count > 0)
}

fn delete_above(tx: &Transaction<'_>, table: &str, head: u64) -> anyhow::Result<usize> {
    if !has_table(tx, table)? {
        return Ok(0);
    }
    tx.execute(
        &format!("DELETE FROM {table} WHERE position > ?1"),
        params![opdb::clamp_i64(head)],
    )
    .with_context(|| format!("discarding `{table}` rows above the head"))
}

fn lower(tx: &Transaction<'_>, table: &str, column: &str, head: u64) -> anyhow::Result<usize> {
    if !has_table(tx, table)? {
        return Ok(0);
    }
    tx.execute(
        &format!("UPDATE {table} SET {column} = ?1 WHERE {column} > ?1"),
        params![opdb::clamp_i64(head)],
    )
    .with_context(|| format!("lowering `{table}.{column}` to the head"))
}

/// How many subject keys the copy holds. Zero means no master key is needed to read it.
fn count_keys(db: &Path) -> anyhow::Result<u64> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {} read-only", db.display()))?;
    let present: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'subject_key'",
            [],
            |row| row.get(0),
        )
        .context("looking for the key store")?;
    if present == 0 {
        return Ok(0);
    }
    let keys: i64 = conn
        .query_row("SELECT count(*) FROM subject_key", [], |row| row.get(0))
        .context("counting subject keys")?;
    Ok(keys as u64)
}

/// The masters the copied keys are wrapped under.
///
/// `OpDb::distinct_master_key_ids` is the same query, and is not used here for the reason the
/// module header gives: reaching it means going through `OpDb::open`, which migrates.
fn master_key_ids(db: &Path) -> anyhow::Result<Vec<String>> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {} read-only", db.display()))?;
    let present: i64 = conn
        .query_row(
            "SELECT count(*) FROM pragma_table_info('subject_key') WHERE name = 'master_key_id'",
            [],
            |row| row.get(0),
        )
        .context("looking for the key store")?;
    if present == 0 {
        return Ok(Vec::new());
    }
    let mut statement = conn
        .prepare(
            "SELECT DISTINCT master_key_id FROM subject_key WHERE master_key_id IS NOT NULL \
             ORDER BY master_key_id",
        )
        .context("preparing the master key query")?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))
        .context("reading the master key ids")?
        .collect::<Result<Vec<_>, _>>()
        .context("reading the master key ids")?;
    Ok(ids)
}

/// Put the snapshot in place.
///
/// The stale sidecars go first. A target that has been booted once, by `hekla verify`, and
/// whose boot did not shut down cleanly has a `-wal` beside its database, and a fresh database
/// renamed in next to another database's WAL is corrupt. The old WAL is of no interest in
/// either case: the database it belongs to is being replaced whole.
fn install(incoming: &Path, target: &Path) -> anyhow::Result<()> {
    let db = target.join("hekla.db");
    for suffix in ["-wal", "-shm"] {
        let stray = target.join(format!("hekla.db{suffix}"));
        if stray.exists() {
            fs::remove_file(&stray).with_context(|| format!("removing {}", stray.display()))?;
        }
    }
    fs::rename(incoming, &db)
        .with_context(|| format!("renaming {} into place", incoming.display()))?;
    // The rename, not the bytes: `snapshot_state` already synced those through SQLite.
    File::open(target)
        .and_then(|dir| dir.sync_all())
        .with_context(|| format!("syncing {}", target.display()))?;
    Ok(())
}

// --- the target ------------------------------------------------------------

/// Whether `dir` is the thing this command copies: both halves of one, so a project directory
/// (which has an `events/` of heklang modules) is not mistaken for one.
fn is_data_dir(dir: &Path) -> bool {
    dir.join("events").is_dir() && dir.join("hekla.db").is_file()
}

/// Whether `target` holds nothing a backup would be overwriting.
///
/// The lock file and a part-copied leftover do not count. Both are this command's own
/// litter, and a first run that failed after taking the lock would otherwise leave a target
/// that every later run refuses: non-empty, and with no manifest yet to say what it is.
fn is_empty(target: &Path) -> anyhow::Result<bool> {
    if !target.exists() {
        return Ok(true);
    }
    anyhow::ensure!(target.is_dir(), "{} is not a directory", target.display());
    let entries = fs::read_dir(target).with_context(|| format!("reading {}", target.display()))?;
    for entry in entries {
        let name = entry
            .with_context(|| format!("reading {}", target.display()))?
            .file_name();
        let name = name.to_string_lossy();
        if name != crate::lock::FILE_NAME && !name.ends_with(INCOMING) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The fields a run reads back out of a target's manifest.
///
/// A subset of [`Report`], and every field optional, because this is read to decide whether a
/// run may proceed: a manifest from another version, or one a dying run left half written,
/// should answer what it can rather than becoming a refusal of its own.
#[derive(Debug, Default, Deserialize)]
struct Previous {
    source: Option<PathBuf>,
    complete: Option<bool>,
    /// Carried through an unfinished claim, because they describe the backup the target still
    /// is until a new state lands: the masters are what it is inert without, and a run
    /// interrupted after the claim would otherwise leave a target naming none.
    #[serde(default)]
    master_key_ids: Vec<String>,
    #[serde(default)]
    keys: u64,
}

/// Whether `dir` is a backup target an interrupted run left behind.
///
/// Worth asking from outside this module, because the documented flow is to verify a backup and
/// an interrupted one *passes*: its key store is older than its log, and a key that is absent
/// is a legitimate state rather than a violation, so the sweep reports a clean directory that
/// is missing keys for everything appended in between. That is the false positive the docs warn
/// about for a hand-rolled copy, arriving by another route.
pub fn interrupted(dir: &Path) -> bool {
    // A manifest that will not parse is a refusal where it matters, inside a run. Here it is
    // simply not a claim this can speak to, and the callers are on their way to doing
    // something else with the directory.
    read_manifest(dir)
        .ok()
        .flatten()
        .is_some_and(|previous| previous.complete == Some(false))
}

/// What a previous run recorded, or `None` when the target holds no manifest at all.
///
/// A manifest that will not parse is an error rather than an absence, which is the opposite
/// of what it used to be. "History unknown" sounds like a reason to carry on, but every guard
/// a target gets reads this: proceeding without it would copy a second deployment's segments
/// in beside the first's, and the result would be a log whose records all pass their CRC.
/// Refusing is loud and has a repair; splicing two logs has neither.
///
/// It cannot be hekla that produced such a file: the write goes through a temporary name and a
/// rename, so there is no window in which a torn one exists under this one.
fn read_manifest(target: &Path) -> anyhow::Result<Option<Previous>> {
    let path = target.join(MANIFEST);
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(None);
    };
    let previous = serde_json::from_str(&text).with_context(|| {
        format!(
            "reading {}; a backup writes it atomically, so this was corrupted from outside. \
             Remove {} and take a fresh backup",
            path.display(),
            target.display()
        )
    })?;
    Ok(Some(previous))
}

/// The unfinished manifest a run writes before it touches the log.
///
/// It carries the *previous* run's head rather than the one about to be copied: until the
/// state lands, what this target holds is still best described by the backup it already was.
fn claim(source: &Path, target: &Path, previous: &Option<Previous>) -> Report {
    Report {
        hekla: env!("CARGO_PKG_VERSION"),
        complete: false,
        taken_at: runtime::now_rfc3339(),
        source: source.to_path_buf(),
        target: target.to_path_buf(),
        // Zero rather than the head the target holds, because the head is read off the log
        // itself and this file is not where that is kept. The masters are, and they are the
        // one thing a target cannot be read without, so they carry over.
        log_head: 0,
        schema_version: 0,
        master_key_ids: previous
            .as_ref()
            .map(|it| it.master_key_ids.clone())
            .unwrap_or_default(),
        keys: previous.as_ref().map_or(0, |it| it.keys),
        files_copied: 0,
        files_reused: 0,
        bytes_copied: 0,
        discarded: Discarded::default(),
        projectors_skipped: Vec::new(),
    }
}

/// Write the manifest, through a temporary name.
///
/// In place it would have a window where the file is truncated, and the file is what says
/// this directory is a backup at all: a crash in that window would leave a target every later
/// run refuses, for a reason that has nothing to do with what the directory holds.
fn write_manifest(target: &Path, report: &Report) -> anyhow::Result<()> {
    let path = target.join(MANIFEST);
    let incoming = target.join(format!("{MANIFEST}{INCOMING}"));
    let text = serde_json::to_string_pretty(report).context("rendering the manifest")?;
    let write = || -> io::Result<()> {
        let mut file = File::create(&incoming)?;
        file.write_all(text.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()
    };
    write().with_context(|| format!("writing {}", incoming.display()))?;
    fs::rename(&incoming, &path)
        .with_context(|| format!("renaming {} into place", incoming.display()))?;
    sync_dir(target)?;
    Ok(())
}

/// The read models in the source directory, by projector name.
fn read_models(source: &Path) -> anyhow::Result<Vec<String>> {
    let dir = source.join("projectors");
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // `{name}.rebuild.db` is a rebuild in flight, which is the same model under another
        // name rather than a second one.
        if let Some(stem) = name.strip_suffix(".db")
            && !stem.ends_with(".rebuild")
        {
            names.push(stem.to_owned());
        }
    }
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cut is an upper bound on what was committed, which is the only direction that is
    /// safe: below it would discard a record the source had committed.
    #[test]
    fn the_extent_is_the_last_non_zero_byte() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("segment");
        let mut file = File::create(&path).unwrap();
        file.write_all(b"hekla").unwrap();
        file.set_len(4096).unwrap();
        drop(file);
        let file = File::open(&path).unwrap();
        assert_eq!(committed_extent(&file, 4096).unwrap(), 5);
    }

    #[test]
    fn an_all_zero_file_has_no_extent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("segment");
        File::create(&path).unwrap().set_len(8192).unwrap();
        let file = File::open(&path).unwrap();
        assert_eq!(committed_extent(&file, 8192).unwrap(), 0);
    }

    /// A copy reads back as the source does: the bytes, then the `fallocate`d zeros.
    #[test]
    fn a_copy_is_the_prefix_and_the_length() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("from"), dir.path().join("to"));
        let mut file = File::create(&from).unwrap();
        file.write_all(b"hekla").unwrap();
        file.set_len(4096).unwrap();
        drop(file);
        copy_prefix(&from, &to, 5, 4096, |_| {}).unwrap();
        assert_eq!(fs::read(&from).unwrap(), fs::read(&to).unwrap());
        assert_eq!(fs::metadata(&to).unwrap().len(), 4096);
    }

    /// Zero-padding is what makes the highest name the highest base position, so the segment
    /// that grows is found without parsing anything.
    #[test]
    fn the_active_segment_is_the_highest_name() {
        let files = vec![
            (PathBuf::from("00000000000000000001.log"), 16),
            (PathBuf::from("00000000000000000912.log"), 16),
            (PathBuf::from("index/00000000000000000001.idx"), 16),
        ];
        assert_eq!(
            active_segment(&files),
            Some(PathBuf::from("00000000000000000912.log"))
        );
    }

    #[test]
    fn a_log_with_no_segments_has_no_active_one() {
        assert_eq!(active_segment(&[]), None);
    }

    /// What makes a repeat backup cheap: the second run moves the segment that grows and
    /// leaves the sealed ones and their indexes alone. Hand-built, because two real segments
    /// is half a gigabyte of log, and this copies files without reading a byte of their
    /// meaning anyway.
    ///
    /// `tests/backup.rs` does it again over a real rolled-over log, built through tephra with
    /// small segments: these pin the counts, that one pins the consequence.
    #[test]
    fn a_second_copy_moves_only_what_can_have_changed() {
        let dir = tempfile::tempdir().unwrap();
        let (source, target) = (dir.path().join("source"), dir.path().join("target"));
        let events = source.join("events");
        fs::create_dir_all(events.join("index")).unwrap();
        fs::write(events.join("00000000000000000001.log"), b"sealed").unwrap();
        fs::write(events.join("00000000000000000912.log"), b"active").unwrap();
        fs::write(events.join("index/00000000000000000001.idx"), b"index").unwrap();
        fs::write(events.join(TEPHRA_LOCK), b"112751").unwrap();

        let quiet = Progress::new(false);
        let first = copy_log(&source, &target, &quiet).unwrap();
        assert_eq!((first.copied, first.reused), (3, 0));
        assert!(
            !target.join("events").join(TEPHRA_LOCK).exists(),
            "a lock file is not data, and the pid in it is somebody else's"
        );

        let second = copy_log(&source, &target, &quiet).unwrap();
        assert_eq!(
            (second.copied, second.reused),
            (1, 2),
            "only the highest base position can have grown"
        );
    }

    /// The clamp's coverage, checked against the schema rather than against itself.
    ///
    /// Every table in a migrated operational database is either one the clamp lowers or one
    /// named here as carrying no tephra position. A migration that adds a table fails this
    /// until somebody decides which it is, which is the only way a list of tables kept in one
    /// module can be made to track a schema kept in another. The cost of getting it wrong is
    /// not a wrong number: it is a position left above the head, and a restored deployment
    /// skipping an effect for an event it has never seen.
    #[test]
    fn every_table_is_clamped_or_known_to_need_no_clamp() {
        const UNPOSITIONED: &[&str] = &["declaration", "subject_key"];

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hekla.db");
        crate::opdb::OpDb::open(&path).unwrap();
        let conn = Connection::open(&path).unwrap();
        let mut statement = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' \
                 AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .unwrap();
        let tables: Vec<String> = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        let clamped: HashSet<&str> = opdb::POSITIONED_TABLES
            .iter()
            .copied()
            .chain(opdb::WATERMARK_COLUMNS.iter().map(|(table, _)| *table))
            .collect();
        for table in &tables {
            assert!(
                clamped.contains(table.as_str()) || UNPOSITIONED.contains(&table.as_str()),
                "the schema has a table `{table}` that the clamp neither lowers nor is told \
                 carries no position. If it holds a tephra position, add it to \
                 `opdb::POSITIONED_TABLES` or `WATERMARK_COLUMNS`; if it does not, name it in \
                 `UNPOSITIONED` here"
            );
        }
        // And the other way, so a table that is renamed or dropped out from under the clamp is
        // caught rather than silently clamping nothing.
        for table in clamped {
            assert!(
                tables.iter().any(|name| name == table),
                "the clamp names `{table}`, which the schema no longer has"
            );
        }
    }

    /// Write `content` into a file of `len` bytes, which is the shape every segment has: data,
    /// then the `fallocate`d zeros it was created with.
    fn padded(path: &Path, content: &[u8], len: u64) {
        let mut file = File::create(path).unwrap();
        file.write_all(content).unwrap();
        file.set_len(len).unwrap();
    }

    /// The case a length check cannot see, and the reason the reuse rule compares extents.
    ///
    /// A segment copied while it was still being appended to is a shorter prefix of a file
    /// whose length never changes, so once it seals, matching on name and length alone would
    /// keep that truncated copy for ever and the backup would be short of committed events for
    /// as long as the target existed.
    #[test]
    fn a_segment_copied_while_it_grew_is_copied_again_once_it_seals() {
        let dir = tempfile::tempdir().unwrap();
        let (source, target) = (dir.path().join("source"), dir.path().join("target"));
        let events = source.join("events");
        fs::create_dir_all(&events).unwrap();
        let first = events.join("00000000000000000001.log");
        padded(&first, b"one", 4096);

        let quiet = Progress::new(false);
        let initial = copy_log(&source, &target, &quiet).unwrap();
        assert_eq!((initial.copied, initial.reused), (1, 0));

        // The writer appends to it and then rolls over, so the file that was active is sealed
        // and the same length it always was.
        padded(&first, b"one two", 4096);
        padded(&events.join("00000000000000000912.log"), b"three", 4096);

        let second = copy_log(&source, &target, &quiet).unwrap();
        assert_eq!(
            (second.copied, second.reused),
            (2, 0),
            "the sealed segment grew since it was copied, so it moves again"
        );
        assert_eq!(
            fs::read(target.join("events/00000000000000000001.log")).unwrap(),
            fs::read(&first).unwrap(),
            "and the copy is whole"
        );

        let third = copy_log(&source, &target, &quiet).unwrap();
        assert_eq!(
            (third.copied, third.reused),
            (1, 1),
            "with nothing changed, only the segment that can grow is moved"
        );
    }

    /// A symlinked segment is a segment. `DirEntry::metadata` does not follow one, so the
    /// length would be the link's own while the copy, which opens the path, followed it: the
    /// result was a segment truncated to the length of its path.
    #[test]
    fn a_symlinked_segment_is_copied_whole() {
        let dir = tempfile::tempdir().unwrap();
        let (source, target) = (dir.path().join("source"), dir.path().join("target"));
        fs::create_dir_all(source.join("events")).unwrap();
        let elsewhere = dir.path().join("moved-to-another-volume.log");
        padded(&elsewhere, b"committed", 40_000);
        std::os::unix::fs::symlink(&elsewhere, source.join("events/00000000000000000001.log"))
            .unwrap();

        let copied = copy_log(&source, &target, &Progress::new(false)).unwrap();
        assert_eq!(copied.bytes, 9, "the data, not the length of the link");
        let landed = target.join("events/00000000000000000001.log");
        assert_eq!(fs::metadata(&landed).unwrap().len(), 40_000);
        assert_eq!(fs::read(&landed).unwrap(), fs::read(&elsewhere).unwrap());
    }

    /// An `events/` with nothing in it is a real state: a boot that died between creating the
    /// directory and initialising the log leaves exactly that, and `is_data_dir` admits it.
    #[test]
    fn a_log_directory_with_nothing_in_it_copies_nothing_and_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let (source, target) = (dir.path().join("source"), dir.path().join("target"));
        fs::create_dir_all(source.join("events")).unwrap();

        let copied = copy_log(&source, &target, &Progress::new(false)).unwrap();
        assert_eq!((copied.copied, copied.reused, copied.bytes), (0, 0, 0));
        assert!(target.join("events").is_dir());
    }

    /// SQLite names its sidecars after the database, so a clamp killed mid-transaction leaves
    /// `hekla.db.incoming-journal`. Left in place, it is a hot journal beside the *next* run's
    /// snapshot of the same name, and SQLite would roll pages from one database back into
    /// another.
    #[test]
    fn a_staged_sidecar_is_swept_with_its_staged_database() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join(format!("hekla.db{INCOMING}"));
        let journal = dir.path().join(format!("hekla.db{INCOMING}-journal"));
        fs::write(&staged, b"half a snapshot").unwrap();
        fs::write(&journal, b"pages from another database").unwrap();

        sweep_incoming(dir.path()).unwrap();
        assert!(!staged.exists());
        assert!(!journal.exists(), "the sidecar is the dangerous half");
    }

    /// A target half-written by a run that died must not be mistaken for a complete one, which
    /// is what the rename buys and what the sweep keeps tidy.
    #[test]
    fn a_part_copied_file_is_never_mistaken_for_a_whole_one() {
        let dir = tempfile::tempdir().unwrap();
        let (source, target) = (dir.path().join("source"), dir.path().join("target"));
        fs::create_dir_all(source.join("events")).unwrap();
        fs::write(source.join("events/00000000000000000001.log"), b"sealed").unwrap();
        fs::create_dir_all(target.join("events")).unwrap();
        // What a run that died mid-copy leaves: the right length, half the bytes, and a name
        // the reuse check does not trust.
        let stray = target.join(format!("events/00000000000000000001.log{INCOMING}"));
        fs::write(&stray, b"sea").unwrap();

        let copied = copy_log(&source, &target, &Progress::new(false)).unwrap();
        assert_eq!((copied.copied, copied.reused), (1, 0));
        assert!(
            !stray.exists(),
            "and the leftover is swept rather than kept"
        );
        assert_eq!(
            fs::read(target.join("events/00000000000000000001.log")).unwrap(),
            b"sealed"
        );
    }
}
