use zerocopy::little_endian::{U16, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use crate::ChildRef;

pub(super) const HEADER_BYTES: usize = 16;
pub(super) const SLOT_BYTES: usize = 8;
pub(super) const INLINE_KEY_BYTES: usize = 4;

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

pub(super) fn encode_kind(child: ChildRef) -> u16 {
    match child {
        ChildRef::Inner(_) => INNER_CHILDREN,
        ChildRef::Leaf(_) => LEAF_CHILDREN,
    }
}
