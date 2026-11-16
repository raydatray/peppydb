//! Record bodies and the `state` file (plan 0029, "On-disk layout").
//!
//! ```text
//! segment: the wal-segments header (magic "PEPPYRLG"), then frames padded
//! to 8 bytes. Frame bodies:
//!   kind u8 (0 blank, 1 normal, 2 membership) | index u64 | term u64 | payload
//!   kind u8 = 3 | committed term u64 | committed index u64
//!
//! state, 64 bytes:
//!   magic "PEPPYRST" (8) | version u32 = 1 | flags u32
//!   | vote_term u64 | vote_node u64 | purged_term u64 | purged_index u64
//!   | reserved (12) | crc u32 (crc32c of bytes 0..60)
//!   flags: 1 vote present, 2 vote names a node, 4 vote committed,
//!          8 purge point present
//! ```
//!
//! All integers are little-endian.

use wal_segments::{Format, u32_at, u64_at};

use super::{EntryKind, LogEntry, LogPos, StoredVote};

/// `kind`, `index`, and `term`, or `kind` and a log position: every body
/// is at least this long.
pub(crate) const MIN_BODY: usize = 17;
/// The largest entry payload.
pub const MAX_PAYLOAD_BYTES: usize = 64 << 20;

/// The Raft log's segment format: frames padded to 8 bytes.
pub(crate) const FORMAT: Format = Format {
    magic: *b"PEPPYRLG",
    version: 1,
    extension: "log",
    min_body: MIN_BODY,
    max_body: MIN_BODY + MAX_PAYLOAD_BYTES,
    align: 8,
};

const COMMIT: u8 = 3;

/// A decoded record body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Record {
    Entry(LogEntry),
    Commit(LogPos),
}

impl EntryKind {
    fn byte(self) -> u8 {
        match self {
            EntryKind::Blank => 0,
            EntryKind::Normal => 1,
            EntryKind::Membership => 2,
        }
    }
}

/// Append one entry frame. Returns the frame's body length.
pub(crate) fn encode_entry(buf: &mut Vec<u8>, entry: &LogEntry) -> usize {
    FORMAT.frame(buf, |buf| {
        buf.push(entry.kind.byte());
        buf.extend(entry.index.to_le_bytes());
        buf.extend(entry.term.to_le_bytes());
        buf.extend(&entry.payload);
    });
    MIN_BODY + entry.payload.len()
}

/// Append one commit-point frame.
pub(crate) fn encode_commit(buf: &mut Vec<u8>, committed: LogPos) {
    FORMAT.frame(buf, |buf| {
        buf.push(COMMIT);
        buf.extend(committed.term.to_le_bytes());
        buf.extend(committed.index.to_le_bytes());
    });
}

/// Decode a record body, or `None` if it is not one.
pub(crate) fn decode_body(body: &[u8]) -> Option<Record> {
    let (kind, a, b) = (body[0], u64_at(body, 1), u64_at(body, 9));
    let kind = match kind {
        0 => EntryKind::Blank,
        1 => EntryKind::Normal,
        2 => EntryKind::Membership,
        COMMIT if body.len() == MIN_BODY => {
            return Some(Record::Commit(LogPos { term: a, index: b }));
        }
        _ => return None,
    };
    Some(Record::Entry(LogEntry {
        index: a,
        term: b,
        kind,
        payload: body[MIN_BODY..].to_vec(),
    }))
}

pub(crate) const STATE_BYTES: usize = 64;
const STATE_MAGIC: &[u8; 8] = b"PEPPYRST";
const STATE_VERSION: u32 = 1;
const HAS_VOTE: u32 = 1;
const HAS_NODE: u32 = 2;
const COMMITTED: u32 = 4;
const HAS_PURGED: u32 = 8;

/// What the `state` file holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct State {
    pub vote: Option<StoredVote>,
    pub purged: Option<LogPos>,
}

pub(crate) fn encode_state(state: &State) -> [u8; STATE_BYTES] {
    let mut bytes = [0; STATE_BYTES];
    let mut flags = 0;
    bytes[..8].copy_from_slice(STATE_MAGIC);
    bytes[8..12].copy_from_slice(&STATE_VERSION.to_le_bytes());
    if let Some(vote) = state.vote {
        flags |= HAS_VOTE;
        bytes[16..24].copy_from_slice(&vote.term.to_le_bytes());
        if let Some(node) = vote.node {
            flags |= HAS_NODE;
            bytes[24..32].copy_from_slice(&node.to_le_bytes());
        }
        if vote.committed {
            flags |= COMMITTED;
        }
    }
    if let Some(purged) = state.purged {
        flags |= HAS_PURGED;
        bytes[32..40].copy_from_slice(&purged.term.to_le_bytes());
        bytes[40..48].copy_from_slice(&purged.index.to_le_bytes());
    }
    bytes[12..16].copy_from_slice(&flags.to_le_bytes());
    let crc = crc32c::crc32c(&bytes[..60]);
    bytes[60..].copy_from_slice(&crc.to_le_bytes());
    bytes
}

pub(crate) fn decode_state(bytes: &[u8]) -> Result<State, &'static str> {
    if bytes.len() != STATE_BYTES {
        return Err("state file has the wrong size");
    }
    if &bytes[..8] != STATE_MAGIC {
        return Err("bad state magic");
    }
    if u32_at(bytes, 60) != crc32c::crc32c(&bytes[..60]) {
        return Err("bad state checksum");
    }
    if u32_at(bytes, 8) != STATE_VERSION {
        return Err("unsupported state version");
    }
    let flags = u32_at(bytes, 12);
    let vote = (flags & HAS_VOTE != 0).then(|| StoredVote {
        term: u64_at(bytes, 16),
        node: (flags & HAS_NODE != 0).then(|| u64_at(bytes, 24)),
        committed: flags & COMMITTED != 0,
    });
    let purged = (flags & HAS_PURGED != 0).then(|| LogPos {
        term: u64_at(bytes, 32),
        index: u64_at(bytes, 40),
    });
    Ok(State { vote, purged })
}
