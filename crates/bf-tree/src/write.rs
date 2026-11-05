use std::sync::atomic::Ordering;

use super::leaf_map::{LeafWrite, Locked, Scratch, with_scratch};
use super::{Tree, TreeError};
use bf_tree_mini_page::{LeafMut, WriteOutcome};
use bf_tree_node_id::LeafId;
use bf_tree_page::{Page, RecordState};

/// A completed split, its separator already installed. Holds the new right
/// leaf's exclusive lock until dropped; drop it before the left leaf's guard.
pub(super) struct Split<'t> {
    pub(super) right: LeafWrite<'t>,
}

impl Tree {
    /// Insert `key` with `value`, or replace its current value.
    ///
    /// Rejects an empty key, and keys or values beyond the configured limits,
    /// before touching any node.
    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<(), TreeError> {
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
    pub fn remove(&self, key: &[u8]) -> Result<(), TreeError> {
        self.check_key(key)?;

        self.write(key, RecordState::Delete)
    }

    /// Buffer the record blindly in the owning leaf's mini-page. When the
    /// mini-page is full at the largest class, merge it into the base page
    /// (splitting the leaf if needed), then buffer the record into the half
    /// that owns the key, under the locks still held. A fresh mini-page at
    /// the smallest fitting class always takes the record.
    fn write(&self, key: &[u8], state: RecordState<'_>) -> Result<(), TreeError> {
        self.with_leaf_write(key, |leaf| match self.leaves.buffer(leaf, key, state)? {
            WriteOutcome::Buffered | WriteOutcome::SkippedDirty => Ok(()),
            WriteOutcome::NeedsMerge => {
                let split = with_scratch(|scratch| self.merge_leaf(leaf, scratch))?;
                let outcome = match split {
                    Some((separator, mut split)) if key >= separator.as_slice() => {
                        self.leaves.buffer(&mut split.right, key, state)?
                    }
                    _ => self.leaves.buffer(leaf, key, state)?,
                };
                match outcome {
                    WriteOutcome::Buffered => Ok(()),
                    // A fresh mini-page takes any admitted record.
                    other => unreachable!("a fresh mini-page returned {other:?}"),
                }
            }
        })
    }

    /// Under the exclusive lock of the reachable leaf `leaf`: fold its
    /// mini-page into the base page, in place or by a split, write the
    /// result through, and free the block. Returns the right half of a
    /// split, still locked, and its separator. Callers: writes, `flush`, and
    /// eviction (plan 0014).
    ///
    /// The block is freed only after the pages are written and the separator
    /// installed, so an error leaves every buffered record in place.
    pub(super) fn merge_leaf<'t>(
        &'t self,
        leaf: &mut LeafWrite<'t>,
        scratch: &mut Scratch,
    ) -> Result<Option<(Vec<u8>, Split<'t>)>, TreeError> {
        let n = self.config.leaf_capacity;
        let Some(mini) = leaf.take_mini() else {
            return Ok(None);
        };
        let result = (|| {
            let base = self.leaves.load_base(leaf, &mut scratch.page[..n])?;
            let mut merge = LeafMut {
                base,
                mini: Some(&mini),
            };
            if merge.merge_fits()? {
                merge.merge_in_place()?;
                self.leaves
                    .write_through(&mut [(leaf.id(), &mut scratch.page[..n])])?;
                return Ok(None);
            }
            let mut separator = Vec::new();
            let split = self.split_leaf(leaf.id(), scratch, |base, right| {
                separator = LeafMut {
                    base,
                    mini: Some(&mini),
                }
                .merge_split(right)?
                .separator;
                Ok(separator.clone())
            })?;
            Ok(Some((separator, split)))
        })();
        match result {
            Ok(split) => {
                self.leaves.circular_buffer.free(mini.block);
                Ok(split)
            }
            Err(error) => {
                leaf.put_mini(Some(mini));
                Err(error)
            }
        }
    }

    /// Split the reachable leaf `left`, whose caller holds its exclusive
    /// lock and whose merged contents in `scratch.page` do not fit: allocate
    /// and lock a fresh right leaf, let `merge` split into `scratch.right`,
    /// write both pages through in one call, and install the separator.
    /// Returns the right leaf still locked.
    ///
    /// On any error after the allocation, including a failed install, the
    /// fresh slot is freed: a written but unreachable right page would
    /// overlap its neighbour at the next open. (A failed install also leaves
    /// the left page already rewritten; the admission rule makes that
    /// unreachable, and shadow paging in plan 0018 removes the window.)
    pub(super) fn split_leaf(
        &self,
        left: LeafId,
        scratch: &mut Scratch,
        merge: impl FnOnce(&mut Page, &mut [u8]) -> Result<Vec<u8>, TreeError>,
    ) -> Result<Split<'_>, TreeError> {
        let n = self.config.leaf_capacity;
        let right = self.leaves.alloc()?;
        let result = (|| {
            let page = zerocopy::FromBytes::mut_from_bytes(&mut scratch.page[..n])
                .expect("scratch holds a page");
            let separator = merge(page, &mut scratch.right[..n])?;
            let mut pages = [
                (left, &mut scratch.page[..n]),
                (right.id(), &mut scratch.right[..n]),
            ];
            self.leaves.write_through(&mut pages)?;
            self.install(left, separator, right.id())
        })();
        match result {
            Ok(()) => Ok(Split { right }),
            Err(error) => {
                self.leaves.discard(right)?;
                Err(error)
            }
        }
    }

    /// Insert `separator -> right` above `left`. The caller holds the
    /// exclusive locks of both leaves. The parent is found by descending with
    /// the separator: it is a key of the right half, so it lies in the left
    /// leaf's old range, which cannot change while the caller holds `left`.
    /// The version is bumped before the caller releases `left`.
    fn install(&self, left: LeafId, separator: Vec<u8>, right: LeafId) -> Result<(), TreeError> {
        let mut inner = self.inner.write();
        let (leaf, path) = inner.locate(&separator);
        debug_assert_eq!(leaf, left, "the separator lies in the left leaf's range");
        inner.propagate(&path, separator, right)?;
        self.inner_version.fetch_add(1, Ordering::Release);
        Ok(())
    }
}
