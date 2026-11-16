//! Record and segment-header encoding, over `wal-segments`' framing
//! (plan 0029), whose bytes are unchanged from plan 0020.
//!
//! ```text
//! segment header, 32 bytes:
//!   magic "PEPPYWAL" (8) | version u32 = 1 | reserved u32 = 0
//!   | first_seq u64 | header_crc u32 (crc32c of bytes 0..24) | zero (4)
//!
//! record:
//!   len u32 | crc u32 | seq u64 | kind u8 | payload
//!   len = byte count of seq + kind + payload, at least 9
//!   crc = crc32c(len bytes ‖ seq ‖ kind ‖ payload)
//!
//! payload by kind:
//!   0 Put     key_len u32 | value_len u32 | key | value
//!   1 Delete  key_len u32 | key
//!   2 Batch   count u32 | count × (kind u8 (0 or 1) | Put or Delete payload)
//! ```
//!
//! All integers are little-endian. `len` is inside the checksum and has a
//! lower bound, so a zero-filled tail (whose CRC32C over nothing is 0) never
//! decodes as a record.

use wal_segments::{Format, u32_at, u64_at};

use super::{Op, Record};

/// The largest record body (`len`): bounds one write or one batch, and the
/// memory that replay and the pending buffer hold for one record.
pub const MAX_RECORD_BYTES: usize = 16 << 20;

pub(crate) use wal_segments::{HEADER_BYTES, PREFIX_BYTES};
/// `seq` and `kind`: the smallest body.
const MIN_BODY: usize = 9;

/// The engine WAL's segment format: unpadded frames.
pub(crate) const FORMAT: Format = Format {
    magic: *b"PEPPYWAL",
    version: 1,
    extension: "wal",
    min_body: MIN_BODY,
    max_body: MAX_RECORD_BYTES,
    align: 1,
};

const PUT: u8 = 0;
const DELETE: u8 = 1;
const BATCH: u8 = 2;

/// The 32-byte header of a segment whose first record has `first_seq`.
pub(crate) fn encode_header(first_seq: u64) -> [u8; HEADER_BYTES] {
    FORMAT.encode_header(first_seq)
}

/// The `first_seq` of a segment header, or why it is not one.
pub(crate) fn decode_header(bytes: &[u8]) -> Result<u64, &'static str> {
    FORMAT.decode_header(bytes)
}

/// Append one encoded single-operation record.
pub(crate) fn encode_op(buf: &mut Vec<u8>, seq: u64, op: Op<'_>) {
    let kind = match op {
        Op::Put { .. } => PUT,
        Op::Delete { .. } => DELETE,
    };
    encode(buf, seq, kind, |buf| encode_payload(buf, op));
}

/// Append one encoded batch record.
pub(crate) fn encode_batch(buf: &mut Vec<u8>, seq: u64, ops: &[Op<'_>]) {
    encode(buf, seq, BATCH, |buf| {
        buf.extend((ops.len() as u32).to_le_bytes());
        for op in ops {
            buf.push(match op {
                Op::Put { .. } => PUT,
                Op::Delete { .. } => DELETE,
            });
            encode_payload(buf, *op);
        }
    });
}

/// The encoded length of a batch record, prefix included.
pub(crate) fn batch_bytes(ops: &[Op<'_>]) -> usize {
    PREFIX_BYTES + MIN_BODY + 4 + ops.iter().map(|op| 1 + payload_bytes(*op)).sum::<usize>()
}

/// The encoded length of a single-operation record, prefix included.
pub(crate) fn op_bytes(op: Op<'_>) -> usize {
    PREFIX_BYTES + MIN_BODY + payload_bytes(op)
}

fn payload_bytes(op: Op<'_>) -> usize {
    match op {
        Op::Put { key, value } => 8 + key.len() + value.len(),
        Op::Delete { key } => 4 + key.len(),
    }
}

fn encode_payload(buf: &mut Vec<u8>, op: Op<'_>) {
    match op {
        Op::Put { key, value } => {
            buf.extend((key.len() as u32).to_le_bytes());
            buf.extend((value.len() as u32).to_le_bytes());
            buf.extend(key);
            buf.extend(value);
        }
        Op::Delete { key } => {
            buf.extend((key.len() as u32).to_le_bytes());
            buf.extend(key);
        }
    }
}

fn encode(buf: &mut Vec<u8>, seq: u64, kind: u8, payload: impl FnOnce(&mut Vec<u8>)) {
    FORMAT.frame(buf, |buf| {
        buf.extend(seq.to_le_bytes());
        buf.push(kind);
        payload(buf);
    });
}

/// Decode the record at the start of `bytes`: the record and the bytes it
/// spans. `None` if the bytes do not hold a whole, intact, well-formed
/// record.
pub(crate) fn decode(bytes: &[u8]) -> Option<(Record<'_>, usize)> {
    let (body, used) = FORMAT.decode_frame(bytes)?;
    Some((decode_body(body)?, used))
}

/// Decode a record body: its sequence, kind, and payload.
pub(crate) fn decode_body(body: &[u8]) -> Option<Record<'_>> {
    let seq = u64_at(body, 0);
    let mut cursor = Cursor(&body[MIN_BODY..]);
    let ops = match body[8] {
        PUT | DELETE => vec![cursor.op(body[8])?],
        BATCH => {
            let count = cursor.u32()? as usize;
            // Each operation takes at least 5 bytes: bound before allocating.
            if count > cursor.0.len() / 5 {
                return None;
            }
            (0..count)
                .map(|_| {
                    let kind = cursor.byte()?;
                    cursor.op(kind)
                })
                .collect::<Option<_>>()?
        }
        _ => return None,
    };
    cursor.0.is_empty().then_some(Record { seq, ops })
}

/// A reader over a record's payload.
struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.0.len() {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }

    fn byte(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32_at(self.take(4)?, 0))
    }

    /// A Put or Delete payload; any other kind (a nested batch) fails.
    fn op(&mut self, kind: u8) -> Option<Op<'a>> {
        match kind {
            PUT => {
                let key_len = self.u32()? as usize;
                let value_len = self.u32()? as usize;
                let key = self.take(key_len)?;
                let value = self.take(value_len)?;
                Some(Op::Put { key, value })
            }
            DELETE => {
                let key_len = self.u32()? as usize;
                Some(Op::Delete {
                    key: self.take(key_len)?,
                })
            }
            _ => None,
        }
    }
}
