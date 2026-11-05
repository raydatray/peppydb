//! The second chance: copying aging blocks to the tail on access.
//!
//! A block near the head is the next victim. If it is accessed there, it is
//! copied to the tail instead (paper §4.3, §5.5), as upstream does at access
//! time. Eviction itself stays strictly FIFO. Writes relocate under their
//! exclusive lock; reads hold only a shared lock, so they return a hint that
//! is applied after the lock is released.

use super::leaf_map::{LeafWrite, Locked};
use super::{Tree, TreeError};
use bf_tree_circular_buffer::Kind;
use bf_tree_mini_page::Mini;
use bf_tree_node_id::LeafId;

/// Cache maintenance that a read under the shared lock found worth doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Hint {
    /// The leaf's block has aged into the second-chance region.
    Relocate,
}

/// A hint and the leaf version it was taken under.
#[derive(Clone, Copy, Debug)]
pub(super) struct Noted {
    leaf: LeafId,
    version: u32,
    hint: Hint,
}

impl Tree {
    /// The hint for a leaf held under any lock: `Relocate` if its block is in
    /// the second-chance region.
    pub(super) fn note(&self, leaf: &impl Locked) -> Option<Noted> {
        let addr = leaf.block_addr()?;
        self.leaves
            .circular_buffer
            .in_second_chance(addr)
            .then(|| Noted {
                leaf: leaf.id(),
                version: leaf.version(),
                hint: Hint::Relocate,
            })
    }

    /// Apply a hint after the shared lock is released: try the exclusive lock
    /// once, and apply the hint only if the leaf is unchanged since it was
    /// noted. A reader never waits for its hint.
    pub(super) fn maintain(&self, noted: Option<Noted>) -> Result<(), TreeError> {
        let Some(noted) = noted else {
            return Ok(());
        };
        let Some(mut leaf) = self.leaves.try_write(noted.leaf) else {
            return Ok(());
        };
        if leaf.version() != noted.version {
            return Ok(());
        }
        match noted.hint {
            Hint::Relocate => self.relocate(&mut leaf),
        }
        Ok(())
    }

    /// Copy the locked leaf's block to the tail if it has aged into the
    /// second-chance region. Best effort: never evicts. The copy keeps the
    /// size class and drops cold clean records; the old block, being in the
    /// region, becomes a tombstone.
    pub(super) fn relocate(&self, leaf: &mut LeafWrite<'_>) {
        let buffer = &self.leaves.circular_buffer;
        let Some(addr) = leaf.block_addr() else {
            return;
        };
        if !buffer.in_second_chance(addr) {
            return;
        }
        if let Some((old, dirty)) = leaf.take_full() {
            let size = old.as_ref().len();
            match buffer.alloc(size, Kind::Full, leaf.id()) {
                Ok(mut fresh) => {
                    fresh.as_mut().copy_from_slice(old.as_ref());
                    leaf.set_full(fresh, dirty);
                    buffer.free(old);
                }
                Err(_) => leaf.set_full(old, dirty),
            }
            return;
        }
        let Some(mini) = leaf.take_mini() else {
            return;
        };
        let size = mini.block.as_ref().len();
        let Ok(block) = buffer.alloc(size, Kind::Mini, leaf.id()) else {
            leaf.put_mini(Some(mini));
            return;
        };
        let mut fresh = Mini::new(block, leaf.id());
        mini.copy_into(&mut fresh, true);
        leaf.put_mini(Some(fresh));
        buffer.free(mini.block);
    }
}
