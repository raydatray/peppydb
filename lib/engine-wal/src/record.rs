//! Record and segment-header encoding.
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

use super::{Op, Record};

/// The largest record body (`len`): bounds one write or one batch, and the
/// memory that replay and the pending buffer hold for one record.
pub const MAX_RECORD_BYTES: usize = 16 << 20;

pub(crate) const HEADER_BYTES: usize = 32;
const MAGIC: &[u8; 8] = b"PEPPYWAL";
const VERSION: u32 = 1;
/// `len` and `crc` before the body.
pub(crate) const PREFIX_BYTES: usize = 8;
/// `seq` and `kind`: the smallest body.
const MIN_BODY: usize = 9;

const PUT: u8 = 0;
const DELETE: u8 = 1;
const BATCH: u8 = 2;

/// The 32-byte header of a segment whose first record has `first_seq`.
pub(crate) fn encode_header(first_seq: u64) -> [u8; HEADER_BYTES] {
    let mut header = [0; HEADER_BYTES];
    header[..8].copy_from_slice(MAGIC);
    header[8..12].copy_from_slice(&VERSION.to_le_bytes());
    header[16..24].copy_from_slice(&first_seq.to_le_bytes());
    let crc = crc32c::crc32c(&header[..24]);
    header[24..28].copy_from_slice(&crc.to_le_bytes());
    header
}

/// The `first_seq` of a segment header, or why it is not one.
pub(crate) fn decode_header(bytes: &[u8]) -> Result<u64, &'static str> {
    let header = bytes.get(..HEADER_BYTES).ok_or("short header")?;
    if &header[..8] != MAGIC {
        return Err("bad magic");
    }
    if header[24..28] != crc32c::crc32c(&header[..24]).to_le_bytes() {
        return Err("bad header checksum");
    }
    if u32_at(header, 8) != VERSION {
        return Err("unsupported version");
    }
    Ok(u64_at(header, 16))
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
    let start = buf.len();
    buf.extend([0; PREFIX_BYTES]);
    buf.extend(seq.to_le_bytes());
    buf.push(kind);
    payload(buf);
    let len = (buf.len() - start - PREFIX_BYTES) as u32;
    buf[start..start + 4].copy_from_slice(&len.to_le_bytes());
    let crc = crc32c::crc32c_append(
        crc32c::crc32c(&len.to_le_bytes()),
        &buf[start + PREFIX_BYTES..],
    );
    buf[start + 4..start + 8].copy_from_slice(&crc.to_le_bytes());
}

/// Decode the record at the start of `bytes`: the record and the bytes it
/// spans. `None` if the bytes do not hold a whole, intact, well-formed
/// record.
pub(crate) fn decode(bytes: &[u8]) -> Option<(Record<'_>, usize)> {
    let prefix = bytes.get(..PREFIX_BYTES)?;
    let len = u32_at(prefix, 0) as usize;
    if !(MIN_BODY..=MAX_RECORD_BYTES).contains(&len) {
        return None;
    }
    let body = bytes.get(PREFIX_BYTES..PREFIX_BYTES + len)?;
    let crc = crc32c::crc32c_append(crc32c::crc32c(&prefix[..4]), body);
    if crc != u32_at(prefix, 4) {
        return None;
    }
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
    cursor
        .0
        .is_empty()
        .then_some((Record { seq, ops }, PREFIX_BYTES + len))
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

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}
