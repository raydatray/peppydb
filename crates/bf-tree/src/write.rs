use zerocopy::FromBytes;

use super::leaf_map::{Locked, with_scratch};
use super::read::Path;
use super::{Tree, TreeError};
use bf_tree_inner::{InnerError, InnerNode};
use bf_tree_node_id::{ChildRef, LeafId};
use bf_tree_page::{KeyRef, RecordRef, RecordState};

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
        let n = self.config.leaf_capacity;
        let separator = {
            let leaves = &self.leaves;
            let leaf = leaves.write(leaf)?;
            with_scratch(|scratch| -> Result<_, TreeError> {
                let page = leaves.load_base(&leaf, &mut scratch.page[..n])?;
                page.check_key(key)?;
                if page.merge_batch_fits(batch.iter().copied())? {
                    page.merge_batch_in_place(batch.iter().copied())?;
                    leaves.write_through(&mut [(leaf.id(), &mut scratch.page[..n])])?;
                    return Ok(None);
                }

                let right = leaves.alloc()?;
                let split = page.merge_batch_split(&mut scratch.right[..n], batch.iter().copied());
                let separator = match split {
                    Ok(split) => split.separator,
                    Err(error) => {
                        leaves.discard(right)?;
                        return Err(error.into());
                    }
                };
                let mut pages = [
                    (leaf.id(), &mut scratch.page[..n]),
                    (right.id(), &mut scratch.right[..n]),
                ];
                if let Err(error) = leaves.write_through(&mut pages) {
                    leaves.discard(right)?;
                    return Err(error);
                }
                Ok(Some((separator, right.id())))
            })?
        };

        match separator {
            None => Ok(()),
            Some((separator, right)) => self.propagate(&path, separator, right),
        }
    }

    /// Insert `separator -> right` into the parent recorded on `path`,
    /// splitting full parents in turn. When the path is exhausted, grow a
    /// new root over the old one.
    fn propagate(
        &mut self,
        path: &Path,
        mut separator: Vec<u8>,
        right: LeafId,
    ) -> Result<(), TreeError> {
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
