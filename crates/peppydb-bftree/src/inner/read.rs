use std::iter::FusedIterator;
use std::ops::Range;

use zerocopy::little_endian::U64;
use zerocopy::{FromBytes, IntoBytes};

use super::InnerNode;
use super::layout::{Slot, decode_child};
use crate::{ChildRef, KeyRef};

/// The local child position and reference chosen for a query.
///
/// `index` is in `0..=len()` and is not a stable handle: it can change after
/// the node is mutated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    pub index: usize,
    pub child: ChildRef,
}

/// A borrowed separator and the child to its right.
///
/// `key.prefix()` is the separator's inline bytes and `key.suffix()` its tail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeparatorRef<'n> {
    pub key: KeyRef<'n>,
    pub right_child: ChildRef,
}

impl InnerNode {
    pub fn first_child(&self) -> ChildRef {
        decode_child(
            self.header.children_kind.get(),
            self.header.first_child.get(),
        )
    }

    /// Index 0 is the first child; index `i > 0` is the right child of
    /// separator `i - 1`.
    pub fn child_at(&self, index: usize) -> Option<ChildRef> {
        match index.checked_sub(1) {
            None => Some(self.first_child()),
            Some(separator) => self
                .slots()
                .get(separator)
                .map(|slot| self.right_child(slot)),
        }
    }

    /// Choose the child whose range contains `key`.
    ///
    /// The index is the number of separators less than or equal to `key`, so
    /// a key equal to a separator routes to that separator's right child. Any
    /// byte slice is a valid query, including an empty one.
    pub fn route(&self, key: &[u8]) -> Route {
        let index = self
            .slots()
            .partition_point(|slot| self.key_of(slot).cmp_bytes(key).is_le());

        let child = self.child_at(index).expect("routing position has a child");

        Route { index, child }
    }

    /// Borrow every separator and its right child in key order without
    /// allocation. The first child is excluded; see [`InnerNode::first_child`].
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            node: self,
            indices: 0..self.len(),
        }
    }

    pub(super) fn slots(&self) -> &[Slot] {
        let (slots, _): (&[Slot], _) =
            FromBytes::ref_from_prefix_with_elems(&self.data, self.len())
                .expect("length is clamped to the buffer");

        slots
    }

    pub(super) fn separator_at(&self, index: usize) -> SeparatorRef<'_> {
        let slot = &self.slots()[index];

        SeparatorRef {
            key: self.key_of(slot),
            right_child: self.right_child(slot),
        }
    }

    /// `slot` must borrow this node's directory so that the inline bytes share
    /// the node's lifetime.
    pub(super) fn key_of<'n>(&'n self, slot: &'n Slot) -> KeyRef<'n> {
        let tail = slot.tail_range(self.as_bytes().len());

        KeyRef::new(slot.inline(), &self.as_bytes()[tail])
    }

    /// Decode a slot's right child; a truncated ID decodes as raw zero.
    fn right_child(&self, slot: &Slot) -> ChildRef {
        let bytes = self.as_bytes();
        let start = slot.child_offset().min(bytes.len());
        let raw = U64::ref_from_prefix(&bytes[start..]).map_or(0, |(raw, _)| raw.get());

        decode_child(self.header.children_kind.get(), raw)
    }
}

impl<'n> IntoIterator for &'n InnerNode {
    type Item = SeparatorRef<'n>;
    type IntoIter = Iter<'n>;

    fn into_iter(self) -> Iter<'n> {
        self.iter()
    }
}

/// Borrowed separators of an [`InnerNode`] in key order.
///
/// Created by [`InnerNode::iter`] or by iterating over `&InnerNode`.
#[derive(Clone, Debug)]
pub struct Iter<'n> {
    node: &'n InnerNode,
    indices: Range<usize>,
}

impl<'n> Iterator for Iter<'n> {
    type Item = SeparatorRef<'n>;

    fn next(&mut self) -> Option<Self::Item> {
        self.indices
            .next()
            .map(|index| self.node.separator_at(index))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.indices.size_hint()
    }
}

impl ExactSizeIterator for Iter<'_> {}

impl FusedIterator for Iter<'_> {}
