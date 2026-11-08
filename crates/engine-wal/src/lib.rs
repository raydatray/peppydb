//! The engine's write-ahead log (plan 0020).
//!
//! An append-only sequence of checksummed records in segment files.
//! Records carry logical operations and a sequence; the log knows nothing
//! about trees. The record format is in [`record`].

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the writer uses it from diff 4")
)]
mod record;
mod replay;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the writer uses it from diff 4")
)]
mod segment;

#[cfg(test)]
mod tests;

pub use record::MAX_RECORD_BYTES;
pub use replay::Replayed;

/// The write-ahead log. Diff 4 adds the writer.
#[derive(Debug)]
pub struct Wal {}

/// One logged change. Keys and values borrow the caller's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op<'a> {
    Put { key: &'a [u8], value: &'a [u8] },
    Delete { key: &'a [u8] },
}

/// A decoded record: one sequence and the operations it carries. A single
/// write has one operation; a batch has zero or more.
#[derive(Debug, PartialEq, Eq)]
pub struct Record<'a> {
    pub seq: u64,
    pub ops: Vec<Op<'a>>,
}

/// A WAL failure.
#[derive(Debug, thiserror::Error)]
pub enum WalError {
    #[error("WAL I/O error during {op}: {source}")]
    Io {
        op: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("WAL segment {segment} is corrupt at offset {offset}: {reason}")]
    Corrupt {
        segment: u64,
        offset: u64,
        reason: &'static str,
    },
    #[error("WAL is missing records: expected sequence {expected}, found {found}")]
    Gap { expected: u64, found: u64 },
    #[error("WAL record of {bytes} bytes exceeds the limit")]
    RecordTooLarge { bytes: usize },
    #[error("the WAL flusher stopped after an error during {op}: {kind}")]
    Failed {
        op: &'static str,
        kind: std::io::ErrorKind,
    },
    #[error("the WAL is closed")]
    Closed,
}
