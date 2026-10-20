use std::ops::Range;

use zerocopy::little_endian::{U16, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::RecordKind;

pub(super) const HEADER_BYTES: usize = 32;
pub(super) const SLOT_BYTES: usize = 8;
pub(super) const PREVIEW_BYTES: usize = 2;

pub(super) const NO_BACKING_LEAF: u64 = u64::MAX;
// Fence slot offsets that encode unbounded lower and upper fences.
pub(super) const LOW_INFINITY: u16 = u16::MAX - 1;
pub(super) const HIGH_INFINITY: u16 = u16::MAX;

const FLAGS_SHIFT: u32 = 12;
pub(super) const COUNT_MASK: u16 = (1 << FLAGS_SHIFT) - 1;
pub(super) const HAS_FENCES: u16 = 1 << (FLAGS_SHIFT + 3);

pub(super) const KIND_SHIFT: u32 = 14;
const KEY_LEN_MASK: u16 = (1 << KIND_SHIFT) - 1;

const REFERENCED_SHIFT: u32 = 15;
const REFERENCED_BIT: u16 = 1 << REFERENCED_SHIFT;
const VALUE_LEN_MASK: u16 = (1 << REFERENCED_SHIFT) - 1;

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
    /// Length of the key prefix stored once in the low fence and omitted from
    /// the high fence and every record. Zero unless both fences are finite.
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
    /// Page-relative offset of the stored key bytes; the value follows them.
    pub(super) offset: U16,
    /// Record kind in the high 2 bits; logical key length in the low 14 bits.
    pub(super) kind_and_key_len: U16,
    /// Reference bit in the high bit; value length in the low 15 bits.
    pub(super) ref_and_value_len: U16,
    /// First `PREVIEW_BYTES` stored key bytes, zero-padded for shorter stored keys.
    pub(super) preview: [u8; PREVIEW_BYTES],
}

const _: () = assert!(size_of::<Slot>() == SLOT_BYTES);

impl Slot {
    /// Encode an unreferenced record. Preflight must bound both lengths.
    pub(super) fn new(offset: u16, key: &[u8], value_len: u16, kind: RecordKind) -> Self {
        let mut preview = [0; PREVIEW_BYTES];
        let len = key.len().min(PREVIEW_BYTES);
        preview[..len].copy_from_slice(&key[..len]);

        Self {
            offset: U16::new(offset),
            kind_and_key_len: U16::new(kind.key_word(key.len() as u16)),
            ref_and_value_len: U16::new(value_len),
            preview,
        }
    }

    /// Logical key length, including prefix bytes that are not stored.
    pub(super) fn key_len(&self) -> usize {
        usize::from(self.kind_and_key_len.get() & KEY_LEN_MASK)
    }

    /// Stored key bytes, which omit the first `omitted` logical bytes.
    /// Callers must exclude infinite fences first.
    pub(super) fn stored_key_range(&self, omitted: usize) -> Range<usize> {
        let start = usize::from(self.offset.get());

        start..start + self.key_len() - omitted
    }

    /// Page-relative range of the full stored key, for slots that omit no
    /// prefix bytes. Valid only while the page prefix is empty.
    pub(super) fn key_range(&self) -> Range<usize> {
        self.stored_key_range(0)
    }

    pub(super) fn value_len(&self) -> usize {
        usize::from(self.ref_and_value_len.get() & VALUE_LEN_MASK)
    }

    pub(super) fn kind(&self) -> RecordKind {
        match self.kind_and_key_len.get() >> KIND_SHIFT {
            0 => RecordKind::Insert,
            1 => RecordKind::Delete,
            2 => RecordKind::Cache,
            3 => RecordKind::Phantom,
            _ => unreachable!("two-bit record kind"),
        }
    }

    pub(super) fn referenced(&self) -> bool {
        self.ref_and_value_len.get() & REFERENCED_BIT != 0
    }

    pub(super) fn set_referenced(&mut self, referenced: bool) {
        let length = self.ref_and_value_len.get() & VALUE_LEN_MASK;

        self.ref_and_value_len
            .set(length | (u16::from(referenced) << REFERENCED_SHIFT));
    }

    pub(super) fn set_kind(&mut self, kind: RecordKind) {
        self.kind_and_key_len
            .set(kind.key_word(self.kind_and_key_len.get() & KEY_LEN_MASK));
    }

    pub(super) fn set_value_len(&mut self, length: u16) {
        self.ref_and_value_len
            .set((self.ref_and_value_len.get() & REFERENCED_BIT) | length);
    }
}
