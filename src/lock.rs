//! An exclusive lock on a data directory.
//!
//! tephra does not lock its segment directory, so two processes opening one data
//! directory corrupt the log with nothing to stop them. `serve` and `verify` both
//! take this lock for their whole run, so the second one fails to start instead.
//!
//! The lock is a dedicated SQLite file holding an open `BEGIN EXCLUSIVE`
//! transaction. SQLite's own file locking does the cross-process work, which is why
//! this needs no new dependency and no stale-PID handling: the lock is held by an
//! open file descriptor, so it dies with the process that took it, however it dies.
//!
//! It lives in its own file rather than in `hekla.db`, because the operational
//! database is written continuously by the effect runtime and an exclusive
//! transaction held across the process lifetime would deadlock against it.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;
use rusqlite::Connection;

/// The lock file inside the data directory.
pub const FILE_NAME: &str = "hekla.lock.db";

/// An exclusive claim on a data directory, released when dropped.
///
/// The transaction is never committed. Dropping the connection rolls it back and
/// frees the file lock, which is exactly the release semantics wanted: the lock
/// exists for its side effect on other processes, not for its contents.
#[derive(Debug)]
pub struct DataDirLock {
    /// Held for its `Drop`. The open `BEGIN EXCLUSIVE` on this connection is the
    /// lock itself, so nothing else reads this field.
    ///
    /// Behind a `Mutex` only to make the lock `Sync`: a `rusqlite::Connection` is
    /// `Send` but not `Sync`, and the runtime that owns this is shared across
    /// threads. Nothing ever takes the mutex.
    _conn: Mutex<Connection>,
    path: PathBuf,
}

impl DataDirLock {
    /// Take the lock on `data_dir`, creating the lock file if needed.
    ///
    /// Fails when another process holds it, which for a `serve` is the "am I
    /// already running?" check and for a `verify` is what keeps it off a live
    /// directory.
    pub fn acquire(data_dir: &Path) -> anyhow::Result<DataDirLock> {
        let path = data_dir.join(FILE_NAME);
        let conn = Connection::open(&path)
            .with_context(|| format!("opening the lock file at {}", path.display()))?;
        // `immediate` would take a reserved lock, which still admits readers.
        // Exclusive is what makes a second acquire fail rather than proceed.
        conn.execute_batch("BEGIN EXCLUSIVE").map_err(|err| {
            anyhow::anyhow!(
                "the data directory at {} is in use by another hekla process ({err}); \
                 stop it, or run against a copy of the directory",
                data_dir.display()
            )
        })?;
        Ok(DataDirLock {
            _conn: Mutex::new(conn),
            path,
        })
    }

    /// The lock file this holds, for diagnostics.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_acquire_fails_while_the_first_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let first = DataDirLock::acquire(dir.path()).unwrap();
        let err = DataDirLock::acquire(dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("in use by another hekla process"),
            "{err:#}"
        );
        drop(first);
    }

    #[test]
    fn dropping_the_lock_releases_it() {
        let dir = tempfile::tempdir().unwrap();
        let first = DataDirLock::acquire(dir.path()).unwrap();
        drop(first);
        DataDirLock::acquire(dir.path()).expect("the lock should be free once dropped");
    }

    #[test]
    fn the_lock_file_lands_in_the_data_directory() {
        let dir = tempfile::tempdir().unwrap();
        let lock = DataDirLock::acquire(dir.path()).unwrap();
        assert_eq!(lock.path(), dir.path().join(FILE_NAME));
        assert!(lock.path().exists());
    }
}
