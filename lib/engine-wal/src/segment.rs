//! Segment files: naming, listing, and creation.
//!
//! A segment is named by the sequence of its first record,
//! `{first_seq:020}.wal`, so a directory listing is the index.

use std::path::{Path, PathBuf};

use super::WalError;
use super::record::FORMAT;

pub(crate) fn io(op: &'static str) -> impl FnOnce(std::io::Error) -> WalError {
    move |source| WalError::Io { op, source }
}

/// The path of the segment whose first record has `first_seq`.
pub(crate) fn path(dir: &Path, first_seq: u64) -> PathBuf {
    FORMAT.path(dir, first_seq)
}

/// Every segment in `dir`, by first sequence. Other files are ignored. An
/// absent directory holds no segments.
pub(crate) fn list(dir: &Path) -> Result<Vec<(u64, PathBuf)>, WalError> {
    FORMAT.list(dir).map_err(io("list segments"))
}

pub(crate) fn sync_dir(dir: &Path) -> Result<(), WalError> {
    wal_segments::sync_dir(dir).map_err(io("sync directory"))
}
