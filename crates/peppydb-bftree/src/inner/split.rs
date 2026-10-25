use std::cmp::Ordering;

use zerocopy::{FromBytes, IntoBytes};

use super::layout::{HEADER_BYTES, entry_size, stored_parts};
use super::{InnerError, InnerNode, SeparatorRef};
use crate::ChildRef;

/// The result of [`InnerNode::split_with_insert`].
#[derive(Debug)]
pub struct InnerSplit<'r> {
    /// The promoted key, which belongs in the parent. Neither output stores it.
    pub separator: Vec<u8>,
    /// The right output, initialized in the caller's storage. Its first child
    /// is the promoted separator's former right child.
    pub right: &'r mut InnerNode,
}

impl InnerNode {
    /// Split this node's separators plus one pending separator into two nodes.
    ///
    /// One separator is promoted for the parent: `self` keeps the separators
    /// before it and `right_storage` receives those after it, with the promoted
    /// separator's right child as its first child. Each side keeps at least one
    /// separator, and the split point balances used bytes. Errors, including
    /// [`InnerError::CannotSplit`], leave both buffers unchanged. The split
    /// neither allocates a node ID nor updates a parent.
    pub fn split_with_insert<'r>(
        &mut self,
        right_storage: &'r mut [u8],
        key: &[u8],
        right_child: ChildRef,
    ) -> Result<InnerSplit<'r>, InnerError> {
        Self::check_capacity(right_storage.len())?;
        let capacity = self.capacity();
        if right_storage.len() != capacity {
            return Err(InnerError::CapacityMismatch);
        }
        self.check_separator(key, right_child)?;
        let position = self
            .search(key)
            .err()
            .ok_or(InnerError::DuplicateSeparator)?;

        let pending = SeparatorRef {
            key: stored_parts(key),
            right_child,
        };
        let count = self.len() + 1;
        let entry = |index: usize| match index.cmp(&position) {
            Ordering::Less => self.separator_at(index),
            Ordering::Equal => pending,
            Ordering::Greater => self.separator_at(index - 1),
        };

        let sizes = (0..count).map(|index| entry_size(entry(index).key.len()));
        let promoted = choose_promotion(sizes, capacity).ok_or(InnerError::CannotSplit)?;

        // `entry` borrows from `self`, which becomes the left output, so build
        // both sides in scratch images first.
        let mut left_image = [0; 4096];
        let mut right_image = [0; 4096];
        let separator = entry(promoted).key.to_vec();

        let left = InnerNode::init(&mut left_image[..capacity], self.first_child())
            .expect("source capacity is valid");
        for index in 0..promoted {
            left.insert_at(index, entry(index));
        }

        let right = InnerNode::init(&mut right_image[..capacity], entry(promoted).right_child)
            .expect("source capacity is valid");
        for (at, index) in (promoted + 1..count).enumerate() {
            right.insert_at(at, entry(index));
        }

        self.as_mut_bytes().copy_from_slice(&left_image[..capacity]);
        right_storage.copy_from_slice(&right_image[..capacity]);
        let right = InnerNode::mut_from_bytes(right_storage).expect("capacity holds the header");

        Ok(InnerSplit { separator, right })
    }
}

/// Choose the promotion index for entries with the given sizes, or `None`.
///
/// Promoting index `p` leaves entries `0..p` on the left and `p + 1..` on the
/// right. Both endpoints are excluded so that each output keeps an entry.
/// Candidates whose outputs exceed `capacity` are discarded; the rest are
/// scored by `(max used, used difference, p)`.
pub(super) fn choose_promotion(
    sizes: impl Iterator<Item = usize> + Clone,
    capacity: usize,
) -> Option<usize> {
    let count = sizes.clone().count();
    let total: usize = sizes.clone().sum();

    sizes
        .enumerate()
        .scan(0, |before, (index, size)| {
            let left = HEADER_BYTES + *before;
            *before += size;
            Some((index, left, HEADER_BYTES + total - *before))
        })
        .filter(|&(index, left, right)| {
            index > 0 && index + 1 < count && left <= capacity && right <= capacity
        })
        .min_by_key(|&(index, left, right)| (left.max(right), left.abs_diff(right), index))
        .map(|(index, ..)| index)
}
