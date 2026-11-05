use zerocopy::FromBytes;

use super::read::Path;
use super::{Tree, TreeError};
use bf_tree_inner::{InnerError, InnerNode};
use bf_tree_node_id::{ChildRef, LeafId};
use bf_tree_page::{KeyRef, Page, RecordRef, RecordState};

impl Tree {
    /// Insert `key` with `value`, or replace its current value.
    ///
    /// Rejects an empty key, and keys or values beyond the configured limits,
    /// before touching any node.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), TreeError> {
        self.check_key(key)?;
        if value.len() > self.config.max_value_len {
            return Err(TreeError::ValueTooLarge);
        }

        self.write(key, RecordState::Insert(value))
    }

    /// Remove `key` if it is present.
    ///
    /// Leaves are never merged: an empty leaf keeps its fences and stays
    /// reachable.
    pub fn remove(&mut self, key: &[u8]) -> Result<(), TreeError> {
        self.check_key(key)?;

        self.write(key, RecordState::Delete)
    }

    /// Merge a one-record batch into the leaf that owns `key`: in place if
    /// the rebuilt leaf fits, otherwise split it. A `Delete` removes the key
    /// and never splits.
    fn write(&mut self, key: &[u8], state: RecordState<'_>) -> Result<(), TreeError> {
        let (leaf, path) = self.locate(key);
        let batch = [RecordRef {
            key: KeyRef::from(key),
            state,
        }];
        // The batch check rejects a key outside the leaf's fences, which
        // would mean a routing bug.
        let page = self.leaf_mut(leaf)?;
        if page.merge_batch_fits(batch.iter().copied())? {
            page.merge_batch_in_place(batch.iter().copied())?;
            return Ok(());
        }

        self.split_and_propagate(leaf, &path, &batch)
    }

    /// Split `leaf` with `batch` merged in, then insert each new separator
    /// into the parent recorded on `path`, splitting full parents in turn.
    /// When the path is exhausted, grow a new root over the old one.
    fn split_and_propagate(
        &mut self,
        leaf: LeafId,
        path: &Path,
        batch: &[RecordRef<'_>],
    ) -> Result<(), TreeError> {
        let right = self.leaves.alloc()?;
        let (left_bytes, right_bytes) = self.leaves.pair_mut(leaf, right)?;
        let page = Page::mut_from_bytes(left_bytes).expect("leaf buffers hold a page header");
        let mut separator = page
            .merge_batch_split(right_bytes, batch.iter().copied())?
            .separator;
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
