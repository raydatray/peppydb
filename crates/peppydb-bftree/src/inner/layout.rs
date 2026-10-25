use std::ops::Range;

use zerocopy::little_endian::{U16, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use crate::{ChildRef, InnerId, KeyRef, LeafId};

pub(super) const HEADER_BYTES: usize = 16;
pub(super) const SLOT_BYTES: usize = 8;
pub(super) const INLINE_KEY_BYTES: usize = 4;
pub(super) const CHILD_BYTES: usize = 8;

const LEAF_CHILDREN: u16 = 0;
const INNER_CHILDREN: u16 = 1;

#[derive(Debug, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub(super) struct Header {
    /// Separators only; the first child is not counted.
    pub(super) count: U16,
    pub(super) capacity: U16,
    /// Free gap between the slot directory and the payloads.
    pub(super) remaining_size: U16,
    /// `LEAF_CHILDREN` or `INNER_CHILDREN`, shared by every child.
    pub(super) children_kind: U16,
    pub(super) first_child: U64,
}

const _: () = assert!(size_of::<Header>() == HEADER_BYTES);

/// A separator's entry in the sorted directory after the header.
#[derive(Clone, Copy, Debug, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub(super) struct Slot {
    /// Node-relative start of the key tail; the right child's ID follows it.
    pub(super) offset: U16,
    /// Full key length, including the inline bytes.
    pub(super) key_len: U16,
    /// Leading key bytes, zero-padded for shorter keys.
    pub(super) inline_key: [u8; INLINE_KEY_BYTES],
}

const _: () = assert!(size_of::<Slot>() == SLOT_BYTES);

impl Slot {
    /// Encode a separator whose key prefix holds exactly its inline bytes.
    /// Preflight must bound the offset and key length.
    pub(super) fn new(offset: u16, key: KeyRef<'_>) -> Self {
        let inline = key.prefix();
        debug_assert_eq!(inline.len(), key.len().min(INLINE_KEY_BYTES));
        let mut inline_key = [0; INLINE_KEY_BYTES];
        inline_key[..inline.len()].copy_from_slice(inline);

        Self {
            offset: U16::new(offset),
            key_len: U16::new(key.len() as u16),
            inline_key,
        }
    }

    pub(super) fn key_len(&self) -> usize {
        usize::from(self.key_len.get())
    }

    /// Meaningful inline key bytes, excluding zero padding.
    pub(super) fn inline(&self) -> &[u8] {
        &self.inline_key[..self.key_len().min(INLINE_KEY_BYTES)]
    }

    /// Node-relative range of the key tail, clamped to a buffer of `len` bytes.
    pub(super) fn tail_range(&self, len: usize) -> Range<usize> {
        let start = usize::from(self.offset.get());
        let end = start + tail_len(self.key_len());

        start.min(len)..end.min(len)
    }

    pub(super) fn child_offset(&self) -> usize {
        usize::from(self.offset.get()) + tail_len(self.key_len())
    }
}

pub(super) fn tail_len(key_len: usize) -> usize {
    key_len.saturating_sub(INLINE_KEY_BYTES)
}

/// Slot, key tail, and right-child bytes for one separator.
pub(super) fn entry_size(key_len: usize) -> usize {
    SLOT_BYTES + tail_len(key_len) + CHILD_BYTES
}

/// View a contiguous key split into its inline bytes and tail, as stored.
pub(super) fn stored_parts(key: &[u8]) -> KeyRef<'_> {
    let (inline, tail) = key.split_at(key.len().min(INLINE_KEY_BYTES));

    KeyRef::new(inline, tail)
}

pub(super) fn encode_kind(child: ChildRef) -> u16 {
    match child {
        ChildRef::Inner(_) => INNER_CHILDREN,
        ChildRef::Leaf(_) => LEAF_CHILDREN,
    }
}

/// Decode a stored child. Any nonzero kind decodes as inner, and the reserved
/// raw value decodes as `u64::MAX - 1`, so decoding is total over any bytes.
/// Valid nodes never store either case.
pub(super) fn decode_child(kind: u16, raw: u64) -> ChildRef {
    let raw = raw.min(u64::MAX - 1);

    match kind {
        LEAF_CHILDREN => ChildRef::Leaf(LeafId::new(raw).expect("raw ID is not reserved")),
        _ => ChildRef::Inner(InnerId::new(raw).expect("raw ID is not reserved")),
    }
}
