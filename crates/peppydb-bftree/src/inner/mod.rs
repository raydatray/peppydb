//! Packed inner (routing) nodes backed by caller-owned storage.
//!
//! A node with `n` strictly increasing, nonempty separators stores `n + 1`
//! children. The first child has its own header field; every separator stores
//! its right child. All children of one node have the same kind.
//!
//! ```
//! use peppydb_bftree::inner::InnerNode;
//! use peppydb_bftree::{ChildRef, LeafId};
//!
//! let leaf = |raw| ChildRef::Leaf(LeafId::new(raw).unwrap());
//!
//! let mut storage = [0; 128];
//! let node = InnerNode::init(&mut storage, leaf(0)).unwrap();
//! assert_eq!(node.len(), 0);
//! assert_eq!(node.child_count(), 1);
//! ```

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use crate::ChildRef;

mod layout;

use layout::{HEADER_BYTES, Header, SLOT_BYTES, encode_kind};

/// An operation rejected before changing any node bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InnerError {
    #[error("node capacity must be a multiple of 64 from 64 through 4096 bytes")]
    InvalidCapacity,
    #[error("split destination capacity must equal the source capacity")]
    CapacityMismatch,
    #[error("separator key must not be empty")]
    EmptyKey,
    #[error("child kind differs from the node's children")]
    ChildKindMismatch,
    #[error("separator does not fit in an empty node")]
    SeparatorTooLarge,
    #[error("separator already exists in the node")]
    DuplicateSeparator,
    #[error("node has insufficient free space for the separator")]
    NodeFull,
    #[error("child index is past the last child")]
    InvalidChildIndex,
    #[error("entries cannot form two nodes that fit and each keep a separator")]
    CannotSplit,
}

/// An inner node viewed in place over caller-owned bytes of any alignment.
///
/// Mutating methods assume the bytes came from [`InnerNode::init`] and this
/// type's operations. Shared-reference methods never panic or read outside the
/// buffer for any bytes, although their results are meaningless for bytes that
/// no valid node contains.
#[derive(Debug, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct InnerNode {
    header: Header,
    data: [u8],
}

impl InnerNode {
    /// Zero the buffer and initialize a node with one child and no separators.
    /// The first child fixes the kind of every later child. An invalid capacity
    /// leaves the buffer unchanged.
    pub fn init(bytes: &mut [u8], first: ChildRef) -> Result<&mut Self, InnerError> {
        let capacity = bytes.len();
        Self::check_capacity(capacity)?;

        bytes.fill(0);

        let node = Self::mut_from_bytes(bytes).expect("capacity holds the header");
        // The validated capacity bounds both encoded sizes to u16.
        node.header.capacity.set(capacity as u16);
        node.header
            .remaining_size
            .set((capacity - HEADER_BYTES) as u16);
        node.header.children_kind.set(encode_kind(first));
        node.header.first_child.set(first.raw());

        Ok(node)
    }

    /// Number of separators, excluding the first child. The stored count is
    /// clamped to the slots that fit in the buffer.
    pub fn len(&self) -> usize {
        usize::from(self.header.count.get()).min(self.data.len() / SLOT_BYTES)
    }

    /// Whether the node has no separators; it still has its first child.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn child_count(&self) -> usize {
        self.len() + 1
    }

    pub fn capacity(&self) -> usize {
        usize::from(self.header.capacity.get())
    }

    /// Contiguous free bytes between the slot directory and the payloads.
    pub fn free_bytes(&self) -> usize {
        usize::from(self.header.remaining_size.get())
    }

    fn check_capacity(capacity: usize) -> Result<(), InnerError> {
        if !(64..=4096).contains(&capacity) || !capacity.is_multiple_of(64) {
            return Err(InnerError::InvalidCapacity);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests;
