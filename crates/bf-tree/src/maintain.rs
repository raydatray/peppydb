//! Cache maintenance after reads, and the second chance.
//!
//! Reads hold only a shared lock, so they note what they found worth doing
//! (an aging block, a promotion) and apply it after releasing the lock.
//!
//! A block near the head is the next victim. If it is accessed there, it is
//! copied to the tail instead (paper §4.3, §5.5), as upstream does at access
//! time. Eviction itself stays strictly FIFO. Writes relocate under their
//! exclusive lock; reads hold only a shared lock, so they return a hint that
//! is applied after the lock is released.

use super::leaf_map::{LeafWrite, Locked, Residency, Scratch, with_scratch};
use super::{Tree, TreeError};
use bf_tree_circular_buffer::{Claim, Kind, Victim};
use bf_tree_mini_page::Mini;
use bf_tree_mini_page::WriteOutcome;
use bf_tree_node_id::LeafId;
use bf_tree_page::RecordState;

/// Cache maintenance that a read under the shared lock found worth doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Hint {
    /// Leave a cached record (`Some`) or a phantom (`None`) for the key.
    Cache {
        key: Vec<u8>,
        value: Option<Vec<u8>>,
    },
    /// Keep the whole page in the circular buffer.
    Promote,
}

/// What a read noted under the shared lock, and the leaf version it saw.
#[derive(Clone, Debug)]
pub(super) struct Noted {
    leaf: LeafId,
    version: u32,
    /// The leaf's block has aged into the second-chance region.
    aging: bool,
    hint: Option<Hint>,
}

impl Noted {
    /// Add a hint to what was noted.
    pub(super) fn with_hint(mut self, hint: Option<Hint>) -> Self {
        self.hint = hint;
        self
    }

    #[cfg(test)]
    pub(super) fn is_aging(&self) -> bool {
        self.aging
    }

    fn is_empty(&self) -> bool {
        !self.aging && self.hint.is_none()
    }
}

/// What [`Tree::promote`] achieved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Promotion {
    Promoted,
    /// The circular buffer had no room for a page without eviction.
    NoRoom,
}

impl Tree {
    /// Note a leaf held under any lock: its version, and whether its block is
    /// in the second-chance region.
    pub(super) fn note(&self, leaf: &impl Locked) -> Noted {
        let aging = leaf
            .block_addr()
            .is_some_and(|addr| self.leaves.circular_buffer.in_second_chance(addr));
        Noted {
            leaf: leaf.id(),
            version: leaf.version(),
            aging,
            hint: None,
        }
    }

    /// Whether a read that the base page serves leaves something behind:
    /// rolls the read rate.
    pub(super) fn roll_read(&self) -> bool {
        self.policy.roll(self.config.read_promotion_percent)
    }

    /// The hint of a promoting read of `key`, which found `value` in the
    /// base page: cache the record or the absence, or keep the page.
    pub(super) fn read_hint(&self, key: &[u8], value: Option<&[u8]>) -> Hint {
        if self.config.read_record_cache {
            Hint::Cache {
                key: key.to_vec(),
                value: value.map(<[u8]>::to_vec),
            }
        } else {
            Hint::Promote
        }
    }

    /// Apply what a read noted, after the shared lock is released: try the
    /// exclusive lock once, and act only if the leaf is unchanged. Relocate
    /// an aging block, then apply the hint. A hint that lacks room is
    /// finished with no lock held. A reader never waits for its hint.
    pub(super) fn maintain(&self, noted: Noted) -> Result<(), TreeError> {
        if noted.is_empty() {
            return Ok(());
        }
        let Some(mut leaf) = self.leaves.try_write(noted.leaf) else {
            return Ok(());
        };
        if leaf.version() != noted.version {
            return Ok(());
        }
        if noted.aging {
            self.relocate(&mut leaf);
        }
        let Some(hint) = noted.hint else {
            return Ok(());
        };
        if self.apply_hint(&mut leaf, &hint)? == Promotion::NoRoom {
            let version = leaf.version();
            drop(leaf);
            self.finish_deferred(noted.leaf, version, &hint)?;
        }
        Ok(())
    }

    fn apply_hint<'t>(
        &'t self,
        leaf: &mut LeafWrite<'t>,
        hint: &Hint,
    ) -> Result<Promotion, TreeError> {
        match hint {
            Hint::Promote => with_scratch(|scratch| self.promote(leaf, scratch)),
            Hint::Cache { key, value } => self.cache_fill(leaf, key, value.as_deref()),
        }
    }

    /// Buffer a clean record for `key` in the locked leaf's mini-page. Never
    /// evicts and never merges: a full mini-page turns the fill into a
    /// promotion. A dirty record for the key wins (`SkippedDirty`).
    fn cache_fill<'t>(
        &'t self,
        leaf: &mut LeafWrite<'t>,
        key: &[u8],
        value: Option<&[u8]>,
    ) -> Result<Promotion, TreeError> {
        if matches!(leaf.residency(), Residency::Full(_)) {
            return Ok(Promotion::Promoted);
        }
        let state = value.map_or(RecordState::Phantom, RecordState::Cache);
        match self.leaves.buffer(leaf, key, state)? {
            None => Ok(Promotion::NoRoom),
            Some(WriteOutcome::NeedsMerge) => with_scratch(|scratch| self.promote(leaf, scratch)),
            Some(WriteOutcome::Buffered | WriteOutcome::SkippedDirty) => Ok(Promotion::Promoted),
        }
    }

    /// Keep the locked leaf's page in the circular buffer as a full page.
    /// Never evicts. A mini-page merges first, in place or by a split; the
    /// left half (this leaf) becomes `Full`.
    pub(super) fn promote<'t>(
        &'t self,
        leaf: &mut LeafWrite<'t>,
        scratch: &mut Scratch,
    ) -> Result<Promotion, TreeError> {
        let n = self.config.leaf_capacity;
        let buffer = &self.leaves.circular_buffer;
        match leaf.residency() {
            Residency::Full(_) => return Ok(Promotion::Promoted),
            // Check before merging: a merge without a page to keep is wasted.
            Residency::Mini { .. } if !buffer.has_room(n) => return Ok(Promotion::NoRoom),
            // The merged left page is left in scratch; a split's right guard
            // drops here.
            Residency::Mini { .. } => drop(self.merge_keeping_page(leaf, scratch)?),
            Residency::Base => {
                self.leaves.load_base(leaf, &mut scratch.page[..n])?;
            }
        }
        let Ok(mut block) = buffer.alloc(n, Kind::Full, leaf.id()) else {
            return Ok(Promotion::NoRoom);
        };
        block.as_mut().copy_from_slice(&scratch.page[..n]);
        leaf.set_full(block, false);
        Ok(Promotion::Promoted)
    }

    /// Finish a hint that lacked room, with no lock held: evict blocks whose
    /// eviction costs no I/O beyond a full page's write-back, then retry the
    /// hint once if the leaf is unchanged. Stops at a mini-page that holds a
    /// dirty record: a hint never pays to merge buffered writes.
    fn finish_deferred(&self, id: LeafId, version: u32, hint: &Hint) -> Result<(), TreeError> {
        let need = match hint {
            Hint::Promote => self.config.leaf_capacity,
            Hint::Cache { .. } => self.leaves.classes.as_slice()[0],
        };
        let buffer = &self.leaves.circular_buffer;
        while !buffer.has_room(need) {
            match buffer.claim_victim() {
                Claim::Victim(victim) if victim.kind == Kind::Full => self.evict(victim)?,
                Claim::Victim(victim) => {
                    if !self.evict_if_clean(victim) {
                        return Ok(());
                    }
                }
                // A hint does not wait for other evictors.
                Claim::Busy | Claim::Empty => return Ok(()),
            }
        }
        let Some(mut leaf) = self.leaves.try_write(id) else {
            return Ok(());
        };
        if leaf.version() == version {
            // No room again: the hint is dropped.
            self.apply_hint(&mut leaf, hint)?;
        }
        Ok(())
    }

    /// Evict a claimed mini-page if it holds no dirty record: free its block
    /// with no I/O and retire the claim. Otherwise, or if its owner is busy,
    /// unclaim it and return `false`.
    pub(super) fn evict_if_clean(&self, victim: Victim) -> bool {
        let buffer = &self.leaves.circular_buffer;
        let Some(mut leaf) = self.leaves.try_write(victim.owner) else {
            buffer.unclaim(victim);
            return false;
        };
        if !self.leaves.holds(&leaf, &victim) {
            drop(leaf);
            buffer.retire(victim);
            return true;
        }
        if !has_clean_mini(&leaf) {
            drop(leaf);
            buffer.unclaim(victim);
            return false;
        }
        let mini = leaf.take_mini().expect("a clean mini-page");
        buffer.free(mini.block);
        drop(leaf);
        buffer.retire(victim);
        true
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

/// Whether the locked leaf has a mini-page that holds no dirty record: one
/// that only repeats the base page, so freeing it loses nothing.
pub(super) fn has_clean_mini(leaf: &impl Locked) -> bool {
    match leaf.residency() {
        Residency::Mini { page, .. } => page.dirty().next().is_none(),
        Residency::Full(_) | Residency::Base => false,
    }
}
