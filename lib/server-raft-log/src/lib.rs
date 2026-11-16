//! The on-disk Raft log (plan 0029).
//!
//! Raft entries, the vote, the purge point, and the committed log id, kept
//! in segment files that survive process crashes and power loss. The log
//! knows nothing about openraft: entries are an index, a term, a kind, and
//! payload bytes. `server-raft` adapts it to openraft.

mod disk;
mod format;
pub mod fs;
mod log;
#[cfg(any(test, feature = "sim"))]
pub mod sim_fs;

#[cfg(test)]
mod tests;

pub use format::MAX_PAYLOAD_BYTES;
pub use fs::{Fs, FsFile, RealFile, RealFs};
pub use log::{LogState, RaftLog, WriteId};
#[cfg(any(test, feature = "sim"))]
pub use sim_fs::{SimFile, SimFs, SimImage};

use std::io;

/// A position in the log: an entry's term and index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LogPos {
    pub term: u64,
    pub index: u64,
}

/// What an entry's payload holds. The log does not interpret it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Blank,
    /// A command, in `server-raft`'s codec.
    Normal,
    /// A membership change, in `server-raft`'s encoding.
    Membership,
}

/// One stored entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    pub index: u64,
    pub term: u64,
    pub kind: EntryKind,
    pub payload: Vec<u8>,
}

impl LogEntry {
    pub fn pos(&self) -> LogPos {
        LogPos {
            term: self.term,
            index: self.index,
        }
    }
}

/// The persisted vote: its term, the candidate it names, and whether a
/// quorum granted it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoredVote {
    pub term: u64,
    pub node: Option<u64>,
    pub committed: bool,
}

/// How a log is laid out and cached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogOptions {
    /// A new segment starts at a group boundary once the active one holds
    /// at least this many bytes.
    pub segment_bytes: u64,
    /// The entry cache's budget.
    pub cache_bytes: usize,
}

impl Default for LogOptions {
    fn default() -> Self {
        Self {
            segment_bytes: 64 << 20,
            cache_bytes: 64 << 20,
        }
    }
}

/// What `open` recovered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Recovered {
    pub vote: Option<StoredVote>,
    pub purged: Option<LogPos>,
    /// The last entry, or `purged` if none remain.
    pub last: Option<LogPos>,
    /// The last commit record, or `purged` if that is higher. A lower
    /// bound of what was committed.
    pub committed: Option<LogPos>,
    /// Bytes of torn tail removed from the last segment.
    pub repaired_bytes: u64,
}

/// A log failure.
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("Raft log I/O error during {op}: {source}")]
    Io {
        op: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("Raft log file {file} is corrupt at offset {offset}: {reason}")]
    Corrupt {
        file: String,
        offset: u64,
        reason: &'static str,
    },
    #[error("the Raft log stopped after an error during {op}: {kind}")]
    Failed {
        op: &'static str,
        kind: io::ErrorKind,
    },
    #[error("Raft log entries must be contiguous: expected index {expected}, found {found}")]
    NotContiguous { expected: u64, found: u64 },
    #[error("Raft log entry of {bytes} payload bytes exceeds the limit")]
    EntryTooLarge { bytes: usize },
}

pub(crate) fn io(op: &'static str) -> impl FnOnce(io::Error) -> LogError {
    move |source| LogError::Io { op, source }
}
