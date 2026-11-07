//! Segment files: naming, listing, and creation.
//!
//! A segment is named by the sequence of its first record,
//! `{first_seq:020}.wal`, so a directory listing is the index.

use std::fs::File;
use std::path::{Path, PathBuf};

use super::WalError;

pub(crate) fn io(op: &'static str) -> impl FnOnce(std::io::Error) -> WalError {
    move |source| WalError::Io { op, source }
}

/// The path of the segment whose first record has `first_seq`.
pub(crate) fn path(dir: &Path, first_seq: u64) -> PathBuf {
    dir.join(format!("{first_seq:020}.wal"))
}

/// Every segment in `dir`, by first sequence. Other files are ignored. An
/// absent directory holds no segments.
pub(crate) fn list(dir: &Path) -> Result<Vec<(u64, PathBuf)>, WalError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io("list segments")(error)),
    };
    let mut segments = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io("list segments"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(digits) = name.strip_suffix(".wal") else {
            continue;
        };
        if digits.len() != 20 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        if let Ok(first_seq) = digits.parse() {
            segments.push((first_seq, entry.path()));
        }
    }
    segments.sort_unstable_by_key(|(first_seq, _)| *first_seq);
    Ok(segments)
}

pub(crate) fn sync_dir(dir: &Path) -> Result<(), WalError> {
    File::open(dir)
        .and_then(|dir| dir.sync_all())
        .map_err(io("sync directory"))
}
