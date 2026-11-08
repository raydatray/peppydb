//! Replay: pass every record from a sequence onwards to the caller, repair
//! a torn tail in the last segment, and refuse anything else.

use std::fs::OpenOptions;
use std::path::Path;

use super::record::{HEADER_BYTES, decode, decode_header};
use super::segment::{self, io};
use super::{Record, Wal, WalError};

/// What a replay found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Replayed {
    /// Highest sequence in the log after repair, replayed or not.
    pub last_seq: Option<u64>,
    /// Records passed to `apply`.
    pub records: u64,
    /// Bytes removed from the end of the last segment.
    pub truncated_bytes: u64,
}

impl Wal {
    /// Pass every record with `seq >= from_seq` to `apply`, in order.
    ///
    /// A torn tail in the last segment is cut off, and the cut is synced
    /// before this returns. Anything else that does not decode is
    /// [`WalError::Corrupt`]: damage in a sealed segment, or a bad record
    /// in the last segment that an intact record follows. Sequences must
    /// increase by one; a missing range at or above `from_seq` is
    /// [`WalError::Gap`]. Segments wholly below `from_seq` (left by a
    /// truncation that did not reach the disk) are not read. Must run
    /// before [`Wal::open`].
    pub fn replay<E: From<WalError>>(
        dir: &Path,
        from_seq: u64,
        mut apply: impl FnMut(Record<'_>) -> Result<(), E>,
    ) -> Result<Replayed, E> {
        // Sequences start at 1.
        let from_seq = from_seq.max(1);
        let segments = segment::list(dir)?;
        let mut replayed = Replayed::default();
        // The last record read, for contiguity; `None` at the start and
        // after skipped segments.
        let mut previous: Option<u64> = None;
        let mut applied_any = false;
        for (index, (first_seq, path)) in segments.iter().enumerate() {
            let last = index + 1 == segments.len();
            let corrupt = |offset: usize, reason| WalError::Corrupt {
                segment: *first_seq,
                offset: offset as u64,
                reason,
            };
            let bytes = std::fs::read(path).map_err(io("read segment"))?;
            match decode_header(&bytes) {
                Ok(named) if named == *first_seq => {}
                Ok(_) => return Err(corrupt(0, "header names another sequence").into()),
                // A crash while the last segment was created: it holds
                // nothing durable.
                Err(_) if last => {
                    std::fs::remove_file(path).map_err(io("remove torn segment"))?;
                    segment::sync_dir(dir)?;
                    break;
                }
                Err(reason) => return Err(corrupt(0, reason).into()),
            }
            let skip = segments
                .get(index + 1)
                .is_some_and(|(next, _)| *next <= from_seq);
            if skip {
                previous = None;
                continue;
            }

            let mut offset = HEADER_BYTES;
            while offset < bytes.len() {
                let Some((record, used)) = decode(&bytes[offset..]) else {
                    if !last {
                        return Err(corrupt(offset, "undecodable record").into());
                    }
                    // A tail only if no intact record follows.
                    let after = previous.unwrap_or(0);
                    let follows = (offset + 1..bytes.len()).any(|at| {
                        decode(&bytes[at..]).is_some_and(|(record, _)| record.seq > after)
                    });
                    if follows {
                        return Err(corrupt(offset, "damage before intact records").into());
                    }
                    let file = OpenOptions::new()
                        .write(true)
                        .open(path)
                        .map_err(io("open segment"))?;
                    file.set_len(offset as u64).map_err(io("truncate tail"))?;
                    file.sync_all().map_err(io("sync truncated segment"))?;
                    replayed.truncated_bytes = (bytes.len() - offset) as u64;
                    break;
                };
                let seq = record.seq;
                match previous {
                    None if seq < *first_seq => {
                        return Err(corrupt(offset, "sequence below the segment name").into());
                    }
                    Some(previous) if seq <= previous => {
                        return Err(corrupt(offset, "sequence does not increase").into());
                    }
                    Some(previous) if seq != previous + 1 => {
                        if offset != HEADER_BYTES {
                            return Err(corrupt(offset, "sequence skips inside a segment").into());
                        }
                        // A missing segment between two others.
                        if seq > from_seq {
                            return Err(WalError::Gap {
                                expected: (previous + 1).max(from_seq),
                                found: seq,
                            }
                            .into());
                        }
                    }
                    _ => {}
                }
                previous = Some(seq);
                replayed.last_seq = Some(seq);
                if seq >= from_seq {
                    if !applied_any && seq != from_seq {
                        return Err(WalError::Gap {
                            expected: from_seq,
                            found: seq,
                        }
                        .into());
                    }
                    applied_any = true;
                    apply(record)?;
                    replayed.records += 1;
                }
                offset += used;
            }
        }
        Ok(replayed)
    }
}
