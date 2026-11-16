//! Checksummed records in numbered segment files (plan 0029).
//!
//! The layer shared by the engine's write-ahead log and the Raft log. It
//! frames opaque bodies and names the files that hold them; it never
//! interprets a body.
//!
//! ```text
//! segment header, 32 bytes:
//!   magic (8) | version u32 | reserved u32 = 0
//!   | first u64 | header_crc u32 (crc32c of bytes 0..24) | zero (4)
//!
//! frame:
//!   len u32 | crc u32 | body | zero padding to a multiple of `align`
//!   len = byte count of the body, without padding
//!   crc = crc32c(len bytes ‖ body)
//! ```
//!
//! All integers are little-endian. `len` is inside the checksum and has a
//! lower bound, so a zero-filled tail (whose CRC32C over nothing is 0) never
//! decodes as a frame. With `align = 8`, every frame starts at an 8-aligned
//! offset, so its 8-byte prefix never crosses a sector boundary.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests;

/// The segment header's size.
pub const HEADER_BYTES: usize = 32;
/// `len` and `crc` before each body.
pub const PREFIX_BYTES: usize = 8;

/// One log's parameters: its magic, version, file extension, body bounds,
/// and frame alignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub magic: [u8; 8],
    pub version: u32,
    /// Segment files are named `{first:020}.{extension}`.
    pub extension: &'static str,
    /// The smallest valid body. Must be at least 1.
    pub min_body: usize,
    /// The largest valid body.
    pub max_body: usize,
    /// Frames are padded to a multiple of this: 1 (no padding) or 8.
    pub align: usize,
}

impl Format {
    /// The header of a segment whose first record is numbered `first`.
    pub fn encode_header(&self, first: u64) -> [u8; HEADER_BYTES] {
        let mut header = [0; HEADER_BYTES];
        header[..8].copy_from_slice(&self.magic);
        header[8..12].copy_from_slice(&self.version.to_le_bytes());
        header[16..24].copy_from_slice(&first.to_le_bytes());
        let crc = crc32c::crc32c(&header[..24]);
        header[24..28].copy_from_slice(&crc.to_le_bytes());
        header
    }

    /// The first record number of a segment header, or why it is not one.
    pub fn decode_header(&self, bytes: &[u8]) -> Result<u64, &'static str> {
        let header = bytes.get(..HEADER_BYTES).ok_or("short header")?;
        if header[..8] != self.magic {
            return Err("bad magic");
        }
        if header[24..28] != crc32c::crc32c(&header[..24]).to_le_bytes() {
            return Err("bad header checksum");
        }
        if u32_at(header, 8) != self.version {
            return Err("unsupported version");
        }
        Ok(u64_at(header, 16))
    }

    /// Append one frame whose body `body` writes. The caller keeps the body
    /// within `min_body..=max_body`.
    pub fn frame(&self, buf: &mut Vec<u8>, body: impl FnOnce(&mut Vec<u8>)) {
        let start = buf.len();
        buf.extend([0; PREFIX_BYTES]);
        body(buf);
        let len = (buf.len() - start - PREFIX_BYTES) as u32;
        buf[start..start + 4].copy_from_slice(&len.to_le_bytes());
        let crc = crc32c::crc32c_append(
            crc32c::crc32c(&len.to_le_bytes()),
            &buf[start + PREFIX_BYTES..],
        );
        buf[start + 4..start + 8].copy_from_slice(&crc.to_le_bytes());
        buf.resize(buf.len() + self.padding(len as usize), 0);
    }

    /// The encoded size of a frame with a `body_len`-byte body, prefix and
    /// padding included.
    pub fn frame_bytes(&self, body_len: usize) -> usize {
        PREFIX_BYTES + body_len + self.padding(body_len)
    }

    fn padding(&self, body_len: usize) -> usize {
        (self.align - (PREFIX_BYTES + body_len) % self.align) % self.align
    }

    /// Decode the frame at the start of `bytes`: its body and the bytes the
    /// frame spans. `None` if the bytes do not hold a whole, intact frame
    /// with zero padding.
    pub fn decode_frame<'a>(&self, bytes: &'a [u8]) -> Option<(&'a [u8], usize)> {
        let prefix = bytes.get(..PREFIX_BYTES)?;
        let len = u32_at(prefix, 0) as usize;
        if !(self.min_body..=self.max_body).contains(&len) {
            return None;
        }
        let body = bytes.get(PREFIX_BYTES..PREFIX_BYTES + len)?;
        let crc = crc32c::crc32c_append(crc32c::crc32c(&prefix[..4]), body);
        if crc != u32_at(prefix, 4) {
            return None;
        }
        let used = self.frame_bytes(len);
        let padding = bytes.get(PREFIX_BYTES + len..used)?;
        padding
            .iter()
            .all(|byte| *byte == 0)
            .then_some((body, used))
    }

    /// Whether an intact frame that `accept` takes starts after `offset`.
    /// Recovery calls this at the first undecodable frame of the last
    /// segment: if one does, the damage is corruption, not a torn tail.
    /// Only offsets a frame can start at are tried.
    pub fn intact_frame_after(
        &self,
        bytes: &[u8],
        offset: usize,
        mut accept: impl FnMut(&[u8]) -> bool,
    ) -> bool {
        let first = (offset + 1).next_multiple_of(self.align);
        (first..bytes.len()).step_by(self.align).any(|at| {
            self.decode_frame(&bytes[at..])
                .is_some_and(|(body, _)| accept(body))
        })
    }

    /// The file name of the segment whose first record is `first`.
    pub fn file_name(&self, first: u64) -> String {
        format!("{first:020}.{}", self.extension)
    }

    /// The path of the segment whose first record is `first`.
    pub fn path(&self, dir: &Path, first: u64) -> PathBuf {
        dir.join(self.file_name(first))
    }

    /// The first record number a segment file name carries, or `None` for
    /// any other name.
    pub fn parse_name(&self, name: &str) -> Option<u64> {
        let digits = name.strip_suffix(self.extension)?.strip_suffix('.')?;
        if digits.len() != 20 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok()
    }

    /// Every segment in `dir`, by first record number. Other files are
    /// ignored. An absent directory holds no segments.
    pub fn list(&self, dir: &Path) -> io::Result<Vec<(u64, PathBuf)>> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut segments = Vec::new();
        for entry in entries {
            let entry = entry?;
            let first = entry
                .file_name()
                .to_str()
                .and_then(|name| self.parse_name(name));
            if let Some(first) = first {
                segments.push((first, entry.path()));
            }
        }
        segments.sort_unstable_by_key(|(first, _)| *first);
        Ok(segments)
    }
}

/// Make the directory's entries (created, renamed, and removed names)
/// durable.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// The little-endian `u32` at `at`.
pub fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

/// The little-endian `u64` at `at`.
pub fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}
