use std::ops::Range;

use zerocopy::little_endian::{U16, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::RecordKind;

/// Encoded header length shared by every `Page`.
pub(super) const HEADER_BYTES: usize = 32;
pub(super) const COUNT_MASK: u16 = 0x0fff;
pub(super) const NO_BACKING_LEAF: u64 = u64::MAX;
pub(super) const SLOT_BYTES: usize = 8;
const KEY_LEN_MASK: u16 = 0x3fff;
const VALUE_LEN_MASK: u16 = 0x7fff;

/// Page header at bytes `0..HEADER_BYTES`.
#[derive(Debug, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub(super) struct Header {
    /// Slot count in the low 12 bits; flags in the high 4 bits.
    pub(super) count_and_flags: U16,
    /// Total page length in bytes.
    pub(super) capacity: U16,
    /// Contiguous free bytes between the slot directory and the payloads.
    pub(super) remaining_size: U16,
    /// Length of the prefix shared by all keys; zero until compression exists.
    pub(super) prefix_len: U16,
    /// Location of the backing leaf, or `NO_BACKING_LEAF`.
    pub(super) backing_leaf: U64,
    /// Log sequence number (LSN); inactive.
    pub(super) lsn: U64,
    /// Snapshot version; inactive.
    pub(super) snapshot_version: U64,
}

const _: () = assert!(size_of::<Header>() == HEADER_BYTES);

/// One entry in the sorted slot directory, which follows the header.
#[derive(Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub(super) struct Slot {
    /// Page-relative offset of the key; the value follows the key.
    pub(super) offset: U16,
    /// Record kind in the high 2 bits; key length in the low 14 bits.
    pub(super) kind_and_key_len: U16,
    /// Reference bit in the high bit; value length in the low 15 bits.
    pub(super) ref_and_value_len: U16,
    /// First two key bytes, zero-padded for one-byte keys.
    pub(super) preview: [u8; 2],
}

const _: () = assert!(size_of::<Slot>() == SLOT_BYTES);

impl Slot {
    /// Encode an unreferenced record. Preflight must bound both lengths.
    pub(super) fn new(offset: u16, key: &[u8], value_len: u16, kind: RecordKind) -> Self {
        Self {
            offset: U16::new(offset),
            kind_and_key_len: U16::new(kind.key_word(key.len() as u16)),
            ref_and_value_len: U16::new(value_len),
            preview: [key[0], key.get(1).copied().unwrap_or(0)],
        }
    }

    /// Page-relative byte range of the key, excluding state bits from its length.
    pub(super) fn key_range(&self) -> Range<usize> {
        let start = usize::from(self.offset.get());
        start..start + usize::from(self.kind_and_key_len.get() & KEY_LEN_MASK)
    }

    /// Value length, excluding the reference bit.
    pub(super) fn value_len(&self) -> usize {
        usize::from(self.ref_and_value_len.get() & VALUE_LEN_MASK)
    }

    pub(super) fn kind(&self) -> RecordKind {
        match self.kind_and_key_len.get() >> 14 {
            0 => RecordKind::Insert,
            1 => RecordKind::Delete,
            2 => RecordKind::Cache,
            3 => RecordKind::Phantom,
            _ => unreachable!("two-bit record kind"),
        }
    }

    pub(super) fn referenced(&self) -> bool {
        self.ref_and_value_len.get() & !VALUE_LEN_MASK != 0
    }

    pub(super) fn set_referenced(&mut self, referenced: bool) {
        let length = self.ref_and_value_len.get() & VALUE_LEN_MASK;
        self.ref_and_value_len
            .set(length | (u16::from(referenced) << 15));
    }
}
