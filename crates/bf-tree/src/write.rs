use std::sync::atomic::Ordering;

use super::leaf_map::{LeafWrite, Locked, Scratch, with_scratch};
use super::{Tree, TreeError};
use bf_tree_circular_buffer::Kind;
use bf_tree_mini_page::{LeafMut, WriteOutcome};
use bf_tree_node_id::LeafId;
use bf_tree_page::{Page, PageError, RecordKind, RecordState};

/// Apply one record to a full page, which is the complete leaf: `Insert`
/// upserts and `Delete` removes; clean kinds change nothing. Returns whether
/// a byte changed. `PageFull` leaves the page unchanged.
fn write_full(page: &mut Page, key: &[u8], state: RecordState<'_>) -> Result<bool, PageError> {
    match state.kind() {
        RecordKind::Insert => {}
        RecordKind::Delete => return page.remove(key),
        RecordKind::Cache | RecordKind::Phantom => {
            page.check_key(key)?;
            return Ok(false);
        }
    }
    match page.try_upsert(key, state) {
        Err(PageError::PageFull) if page.reclaimable_bytes() > 0 => {
            page.compact();
            page.try_upsert(key, state)?;
        }
        result => {
            result?;
        }
    }
    Ok(true)
}

/// The result of one write attempt under the leaf lock.
enum Attempt {
    Done,
    /// The circular buffer could not supply a block. Every guard of the
    /// attempt has been dropped; the caller must make room and route again.
    NeedRoom,
}

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

    /// Buffer the record blindly in the owning leaf's mini-page, making room
    /// in the circular buffer when it is full. Eviction runs with no leaf
    /// lock held, so a full buffer ends the attempt; the write then routes
    /// again.
    fn write(&self, key: &[u8], state: RecordState<'_>) -> Result<(), TreeError> {
        loop {
            match self.write_attempt(key, state)? {
                Attempt::Done => return Ok(()),
                Attempt::NeedRoom => self.make_room(self.leaves.classes.largest())?,
            }
        }
    }

    /// One attempt under the owning leaf's lock. When the mini-page is full
    /// at the largest class, merge it into the base page (splitting the leaf
    /// if needed), keep the merged page as a full page if the configuration
    /// asks for it, then place the record in the half that owns the key,
    /// under the locks still held. Every guard is dropped before it returns.
    fn write_attempt(&self, key: &[u8], state: RecordState<'_>) -> Result<Attempt, TreeError> {
        self.with_leaf_write(key, |leaf| {
            let outcome = match self.place(leaf, key, state)? {
                None => return Ok(Attempt::NeedRoom),
                Some(WriteOutcome::NeedsMerge) => {
                    let split = with_scratch(|scratch| {
                        let split = self.merge_leaf(leaf, scratch)?;
                        if self.config.write_load_full_page {
                            self.keep_merged(leaf, scratch);
                        }
                        Ok::<_, TreeError>(split)
                    })?;
                    match split {
                        Some((separator, mut split)) if key >= separator.as_slice() => {
                            self.place(&mut split.right, key, state)?
                        }
                        _ => self.place(leaf, key, state)?,
                    }
                }
                Some(outcome) => Some(outcome),
            };
            match outcome {
                // The merge is complete; a retry places into the owning half.
                None => Ok(Attempt::NeedRoom),
                Some(WriteOutcome::Buffered | WriteOutcome::SkippedDirty) => {
                    // After the write: a write that grew the mini-page has
                    // already moved it to the tail.
                    self.relocate(leaf);
                    Ok(Attempt::Done)
                }
                // A fresh mini-page takes any admitted record.
                Some(WriteOutcome::NeedsMerge) => {
                    unreachable!("a fresh mini-page needed a merge")
                }
            }
        })
    }

    /// Place one record in a locked leaf: directly into its full page, or
    /// buffered in its mini-page. A full page that cannot take the record is
    /// written back if dirty and released, and the record is buffered.
    /// `None` if the circular buffer had no room.
    fn place(
        &self,
        leaf: &mut LeafWrite<'_>,
        key: &[u8],
        state: RecordState<'_>,
    ) -> Result<Option<WriteOutcome>, TreeError> {
        if let Some(page) = leaf.full_mut() {
            match write_full(page, key, state) {
                Ok(changed) => {
                    if changed {
                        leaf.mark_dirty();
                    }
                    return Ok(Some(WriteOutcome::Buffered));
                }
                Err(PageError::PageFull) => self.leaves.release_full(leaf)?,
                Err(error) => return Err(error.into()),
            }
        }
        self.leaves.buffer(leaf, key, state)
    }

    /// Keep the page that `merge_leaf` just left in `scratch.page` as the
    /// leaf's full page, if a block fits without eviction.
    fn keep_merged(&self, leaf: &mut LeafWrite<'_>, scratch: &mut Scratch) {
        let n = self.config.leaf_capacity;
        let Ok(mut block) = self.leaves.circular_buffer.alloc(n, Kind::Full, leaf.id()) else {
            return;
        };
        block.as_mut().copy_from_slice(&scratch.page[..n]);
        leaf.set_full(block, false);
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
