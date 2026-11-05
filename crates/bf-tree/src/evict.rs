//! Making room in the circular buffer by evicting its oldest blocks.
//!
//! A writer that finds the circular buffer full releases every leaf lock,
//! then evicts with no lock held: each eviction claims the oldest live
//! block, locks its owner by ID, and merges the owner's mini-page into its
//! base page through `merge_leaf`. Holding no lock while evicting rules out
//! both a self-deadlock (the victim is the writer's own leaf) and an ABBA
//! deadlock between evicting writers.

use super::leaf_map::{Locked, Residency, with_scratch};
use super::{Tree, TreeError};
use bf_tree_circular_buffer::{Claim, Victim};

/// What one call to [`Tree::evict_one`] achieved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Eviction {
    Evicted,
    /// Every live block is claimed by another evictor.
    Busy,
    /// The circular buffer holds no live block.
    Empty,
}

impl Tree {
    /// Evict until a block of `need` bytes, plus a slack, fits at the tail.
    /// The caller holds no leaf lock.
    ///
    /// Room appears only when the head advances, so an eviction behind a
    /// slow claim at the head makes no room until that claim retires; the
    /// loop then sees `Busy` and yields. `Empty` means no live block is left,
    /// and the capacity rule guarantees that any allocation then fits.
    pub(super) fn make_room(&self, need: usize) -> Result<(), TreeError> {
        // Evict a batch: room for `need` plus a slack of 1/16 of the
        // capacity. Without slack, the buffer runs full under write pressure
        // and relocations, which never evict, find no room, so the second
        // chance never happens. Upstream gets slack the same way, by freeing
        // about 1 KiB per eviction round (`TARGET_EVICT_SIZE`).
        let target = need + self.leaves.circular_buffer.capacity() / 16;
        while !self.leaves.circular_buffer.has_room(target) {
            match self.evict_one()? {
                Eviction::Evicted => {}
                Eviction::Busy => std::thread::yield_now(),
                Eviction::Empty => break,
            }
        }
        Ok(())
    }

    /// Claim the oldest live block and evict it.
    pub(super) fn evict_one(&self) -> Result<Eviction, TreeError> {
        match self.leaves.circular_buffer.claim_victim() {
            Claim::Victim(victim) => self.evict(victim).map(|()| Eviction::Evicted),
            Claim::Busy => Ok(Eviction::Busy),
            Claim::Empty => Ok(Eviction::Empty),
        }
    }

    /// Evict a claimed block. The caller holds no leaf lock.
    ///
    /// If the owner no longer holds the block (it grew, merged, or was
    /// relocated after the claim and freed it), the claim is only retired.
    /// Otherwise the owner's mini-page merges into its base page, which
    /// frees the block, and the claim retires. A failed merge leaves the
    /// mini-page in place and returns the claim, so the head never sticks.
    pub(super) fn evict(&self, victim: Victim) -> Result<(), TreeError> {
        let mut leaf = match self.leaves.write(victim.owner) {
            Ok(leaf) => leaf,
            Err(error) => {
                self.leaves.circular_buffer.unclaim(victim);
                return Err(error);
            }
        };
        if !self.leaves.holds(&leaf, &victim) {
            drop(leaf);
            self.leaves.circular_buffer.retire(victim);
            return Ok(());
        }
        // A full page: write it back if dirty, then free it.
        if let Residency::Full(_) = leaf.residency() {
            return match self.leaves.release_full(&mut leaf) {
                Ok(()) => {
                    drop(leaf);
                    self.leaves.circular_buffer.retire(victim);
                    Ok(())
                }
                Err(error) => {
                    drop(leaf);
                    self.leaves.circular_buffer.unclaim(victim);
                    Err(error)
                }
            };
        }
        match with_scratch(|scratch| self.merge_leaf(&mut leaf, scratch)) {
            Ok(split) => {
                // A split's right guard drops before the owner's guard.
                drop(split);
                drop(leaf);
                self.leaves.circular_buffer.retire(victim);
                Ok(())
            }
            Err(error) => {
                drop(leaf);
                self.leaves.circular_buffer.unclaim(victim);
                Err(error)
            }
        }
    }
}
