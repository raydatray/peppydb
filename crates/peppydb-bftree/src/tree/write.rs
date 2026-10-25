use zerocopy::FromBytes;

use super::read::Path;
use super::{Tree, TreeError};
use crate::inner::{InnerError, InnerNode};
use crate::{ChildRef, LeafId, Page, PageError, RecordState, UpsertOutcome};

/// The result of writing into one leaf without splitting it.
enum LeafWrite {
    Done(UpsertOutcome),
    /// The leaf must split with the record; report this outcome afterward.
    MustSplit(UpsertOutcome),
}

impl Tree {
    /// Insert `key` with `value`, or replace its current value.
    ///
    /// Rejects an empty key, and keys or values beyond the configured limits,
    /// before touching any node.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<UpsertOutcome, TreeError> {
        self.check_key(key)?;
        if value.len() > self.config.max_value_len {
            return Err(TreeError::ValueTooLarge);
        }

        let (leaf, path) = self.locate(key);
        let state = RecordState::Insert(value);
        match self.try_upsert_in_leaf(leaf, key, state)? {
            LeafWrite::Done(outcome) => Ok(outcome),
            LeafWrite::MustSplit(outcome) => {
                self.split_and_propagate(leaf, &path, key, state)?;
                Ok(outcome)
            }
        }
    }

    /// Physically remove `key`; return whether it was present.
    ///
    /// Leaves are never merged: an empty leaf keeps its fences and stays
    /// reachable.
    pub fn remove(&mut self, key: &[u8]) -> Result<bool, TreeError> {
        self.check_key(key)?;

        let leaf = self.find_leaf(key);
        Ok(self.leaf_mut(leaf)?.remove(key)?)
    }

    /// Write `key` into `leaf` without splitting, compacting if its holes make
    /// room. If it returns `MustSplit`, any old version of `key` has already
    /// been removed.
    fn try_upsert_in_leaf(
        &mut self,
        leaf: LeafId,
        key: &[u8],
        state: RecordState<'_>,
    ) -> Result<LeafWrite, TreeError> {
        let page = self.leaf_mut(leaf)?;
        match page.try_upsert(key, state) {
            Err(PageError::PageFull) => {}
            result => return Ok(LeafWrite::Done(result?)),
        }

        // Removing first frees the old version's bytes for the space check, and
        // turns a replacement during a split into an insert.
        let replaced = page.remove(key)?;
        let outcome = match replaced {
            true => UpsertOutcome::Replaced,
            false => UpsertOutcome::Inserted,
        };
        let needed = page.record_size(key, state)?;
        if needed <= page.free_bytes() + page.reclaimable_bytes() {
            if needed > page.free_bytes() {
                page.compact();
            }

            page.try_insert_record(key, state)?;

            return Ok(LeafWrite::Done(outcome));
        }

        Ok(LeafWrite::MustSplit(outcome))
    }

    /// Split `leaf` with the pending record, then insert each new separator
    /// into the parent recorded on `path`, splitting full parents in turn.
    /// When the path is exhausted, grow a new root over the old one.
    fn split_and_propagate(
        &mut self,
        leaf: LeafId,
        path: &Path,
        key: &[u8],
        state: RecordState<'_>,
    ) -> Result<(), TreeError> {
        let right = self.leaves.alloc()?;
        let (left_bytes, right_bytes) = self.leaves.pair_mut(leaf, right)?;
        let page = Page::mut_from_bytes(left_bytes).expect("leaf buffers hold a page header");
        let mut separator = page.split_with_insert(right_bytes, key, state)?.separator;
        let mut child = ChildRef::Leaf(right);

        for &(parent, _) in path.iter().rev() {
            match self.inner_mut(parent).try_insert(&separator, child) {
                Err(InnerError::NodeFull) => {}
                result => return Ok(result?),
            }

            let right = self.inner.alloc();
            let (left_bytes, right_bytes) = self.inner.pair_mut(parent, right);

            let node =
                InnerNode::mut_from_bytes(left_bytes).expect("inner buffers hold a node header");

            separator = node
                .split_with_insert(right_bytes, &separator, child)?
                .separator;

            child = ChildRef::Inner(right);
        }

        let root = self.inner.alloc();
        let node = InnerNode::init(self.inner.get_mut(root), self.root)?;
        node.try_insert(&separator, child)?;
        self.root = ChildRef::Inner(root);

        Ok(())
    }
}
