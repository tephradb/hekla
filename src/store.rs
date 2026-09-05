//! The event log, as the process holding it is allowed to see it.
//!
//! hekla opens tephra two ways. A server owns the log: it holds the data-directory lock,
//! runs the write coordinator, and appends. `hekla plan --replay` only reads, and it
//! reads a directory a server is writing right now, so it opens a [`Follower`] instead:
//! read-only descriptors, no lock, nothing created and nothing deleted.
//!
//! Both hand out a [`ReadHandle`], and every read hekla makes goes through one, so this
//! type is what lets one runtime serve both without a second copy of the read paths.
//!
//! Two operations are not "reading", and both refuse rather than assume. Appending needs
//! a writer, which a follower is not. Subscribing needs a watermark that advances, which
//! a follower's does only when something calls `refresh`; hekla opens a follower for one
//! fixed prefix and never refreshes it, so a subscription over one would park forever on
//! a tip that cannot move. Both refuse here rather than being left to the fact that no
//! current caller reaches them, and both explain themselves here too: the invariant is
//! this module's, so the sentence about it is as well.

use std::sync::Arc;

use tephra::read::Reads;
use tephra::{Follower, Position, Query, ReadHandle, Subscription, WriteHandle};

/// A handle on the log, which may or may not be able to extend or follow it.
#[derive(Clone)]
pub struct Store {
    reader: ReadHandle,
    writer: Option<WriteHandle>,
    /// The follower this reader came from, held for as long as the reader is.
    ///
    /// Not load-bearing today: `Follower` has no `Drop`, and the snapshot it published
    /// holds an `Arc` on every segment and index the handle reads through, so the reader
    /// is self-sufficient once it exists. That is tephra's internal arrangement rather
    /// than a promise it makes, and a `Store` that outlives its follower would depend on
    /// it silently. Holding the `Arc` costs a pointer and removes the dependency.
    _follower: Option<Arc<Follower>>,
}

impl Store {
    /// The log as its writer sees it.
    pub fn writing(writer: WriteHandle) -> Store {
        Store {
            reader: writer.reader(),
            writer: Some(writer),
            _follower: None,
        }
    }

    /// The log as a second process sees it: one committed prefix, fixed at the moment
    /// `follower` was opened, and no way to add to it.
    pub fn following(follower: Arc<Follower>) -> Store {
        Store {
            reader: follower.reader(),
            writer: None,
            _follower: Some(follower),
        }
    }

    /// The writer, or an error naming what wanted one.
    ///
    /// `role` completes "a {role} needs ...", so it is a noun phrase for the thing that
    /// cannot proceed: `"an effect"`, `"a projector"`. Written once here rather than at
    /// each call site, which is how the three call sites this replaced had already
    /// drifted into three spellings of the same sentence.
    pub fn writer(&self, role: &str) -> anyhow::Result<&WriteHandle> {
        self.writer
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("{}", read_only(role, "append to")))
    }

    pub fn read(&self, query: &Query, after: Position, limit: Option<u64>) -> Reads {
        self.reader.read(query, after, limit)
    }

    pub fn read_back(&self, query: &Query, before: Position, limit: Option<u64>) -> Reads {
        self.reader.read_back(query, before, limit)
    }

    /// The last readable position. For a follower this is the tip of the prefix it
    /// pinned when it opened, which is why nothing that reads through one may assume a
    /// position it learned elsewhere is in range.
    pub fn head(&self) -> Position {
        self.reader.head()
    }

    /// A subscription over this log, or an error naming what wanted one.
    ///
    /// A subscription is repeated reads off a moving watermark, so it needs something to
    /// move it. A writer does that at each commit; a follower's moves only on `refresh`,
    /// and hekla never refreshes one. Subscribing to a fixed prefix would block on a tip
    /// that never changes and a store that is never closed. See [`Store::writer`] for
    /// what `role` reads as.
    pub fn subscribe(
        &self,
        role: &str,
        query: Query,
        after: Position,
    ) -> anyhow::Result<Subscription> {
        match &self.writer {
            Some(writer) => Ok(writer.subscribe(query, after)),
            None => Err(anyhow::anyhow!("{}", read_only(role, "follow"))),
        }
    }
}

/// The sentence both refusals end on. `need` completes "a log it can ...".
fn read_only(role: &str, need: &str) -> String {
    format!("{role} needs a log it can {need}, and this one is open for reading only")
}
