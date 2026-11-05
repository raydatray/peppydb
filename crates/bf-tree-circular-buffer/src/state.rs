//! The circular buffer's metadata, kept behind its mutex. Safe code only.

use std::collections::VecDeque;

use super::Metrics;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BlockState {
    /// Held by its owner through a `Block`.
    Live,
    /// Freed outside the second-chance region; on a free list for reuse.
    Freed,
    /// Freed, never reused; dead until the head passes it.
    Tombstone,
    /// Covers the end of the slab when a block would cross it; never had a
    /// handle.
    Filler,
}

impl BlockState {
    /// Whether the head may pass the block.
    fn is_dead(self) -> bool {
        match self {
            Self::Live => false,
            Self::Freed | Self::Tombstone | Self::Filler => true,
        }
    }
}

#[derive(Debug)]
pub(super) struct BlockMeta {
    /// Logical address of the block's first header byte.
    pub(super) addr: usize,
    /// Logical bytes the block occupies: header plus charged size, or the
    /// rest of the slab for a filler.
    pub(super) span: usize,
    pub(super) state: BlockState,
}

#[derive(Debug)]
pub(super) struct State {
    /// Blocks from head to tail in allocation order; index = seq - first_seq.
    blocks: VecDeque<BlockMeta>,
    first_seq: u64,
    pub(super) head: usize,
    pub(super) tail: usize,
    /// Live blocks. The slab is leaked on drop unless this is 0.
    pub(super) outstanding: usize,
    /// Per size class, ascending: sequence numbers of `Freed` blocks. An entry
    /// whose block the head has passed is stale (`seq < first_seq`).
    free: Vec<Vec<u64>>,
    /// Number of stale entries in `free`.
    stale: usize,
    pub(super) metrics: Metrics,
}

impl State {
    pub(super) fn new(classes: usize) -> Self {
        Self {
            blocks: VecDeque::new(),
            first_seq: 0,
            head: 0,
            tail: 0,
            outstanding: 0,
            free: vec![Vec::new(); classes],
            stale: 0,
            metrics: Metrics::default(),
        }
    }

    /// Whether the block at `addr` is in the second-chance region: at least
    /// `threshold` bytes behind the tail.
    pub(super) fn in_region(&self, addr: usize, threshold: usize) -> bool {
        self.tail.saturating_sub(addr) >= threshold
    }

    /// Whether `seq` is the block at the head.
    pub(super) fn is_head(&self, seq: u64) -> bool {
        seq == self.first_seq
    }

    /// Make the live block `seq` a hole on the free list of `class`.
    pub(super) fn add_hole(&mut self, seq: u64, class: usize) {
        let meta = self.get_mut(seq).expect("caller checked the block");
        meta.state = BlockState::Freed;
        self.free[class].push(seq);
    }

    /// Take a hole of `class` that lies outside the second-chance region and
    /// make it live. Holes that have aged into the region become tombstones
    /// on the way. Returns the hole's sequence number and logical address.
    pub(super) fn take_hole(&mut self, class: usize, threshold: usize) -> Option<(u64, usize)> {
        while let Some(seq) = self.free[class].pop() {
            if seq < self.first_seq {
                // The head passed this hole; its space is already free.
                self.stale -= 1;
                continue;
            }
            let tail = self.tail;
            let meta = self
                .get_mut(seq)
                .expect("entries at or above first_seq exist");
            debug_assert_eq!(meta.state, BlockState::Freed);
            if tail.saturating_sub(meta.addr) >= threshold {
                meta.state = BlockState::Tombstone;
                self.metrics.tombstones += 1;
                continue;
            }
            meta.state = BlockState::Live;
            return Some((seq, meta.addr));
        }
        None
    }
    pub(super) fn used(&self) -> usize {
        self.tail - self.head
    }

    pub(super) fn get(&self, seq: u64) -> Option<&BlockMeta> {
        let index = usize::try_from(seq.checked_sub(self.first_seq)?).ok()?;
        self.blocks.get(index)
    }

    pub(super) fn get_mut(&mut self, seq: u64) -> Option<&mut BlockMeta> {
        let index = usize::try_from(seq.checked_sub(self.first_seq)?).ok()?;
        self.blocks.get_mut(index)
    }

    /// The filler a bump allocation of `span` bytes needs first, so that the
    /// block does not cross the end of a slab of `capacity` bytes.
    fn filler(&self, capacity: usize, span: usize) -> usize {
        let offset = self.tail & (capacity - 1);
        if offset + span > capacity {
            capacity - offset
        } else {
            0
        }
    }

    /// Whether a bump allocation of `span` bytes fits now.
    pub(super) fn fits(&self, capacity: usize, span: usize) -> bool {
        self.used() + self.filler(capacity, span) + span <= capacity
    }

    /// Reserve `span` bytes at the tail, after a filler if needed, and return
    /// the new block's sequence number and logical address. `None` if the
    /// budget does not allow it; nothing changes then.
    pub(super) fn bump(&mut self, capacity: usize, span: usize) -> Option<(u64, usize)> {
        if !self.fits(capacity, span) {
            return None;
        }

        let filler = self.filler(capacity, span);
        if filler > 0 {
            self.push(filler, BlockState::Filler);
            self.metrics.fillers += 1;
        }
        let addr = self.tail;
        let seq = self.push(span, BlockState::Live);
        Some((seq, addr))
    }

    fn push(&mut self, span: usize, state: BlockState) -> u64 {
        let seq = self.first_seq + self.blocks.len() as u64;
        self.blocks.push_back(BlockMeta {
            addr: self.tail,
            span,
            state,
        });
        self.tail += span;
        seq
    }

    /// Move the head past every dead block at the front. Holes it passes
    /// leave stale free-list entries; when those exceed half of all entries,
    /// every list is filtered, so the cost is amortized over the frees that
    /// created them.
    pub(super) fn advance_head(&mut self) {
        while let Some(front) = self.blocks.front()
            && front.state.is_dead()
        {
            if front.state == BlockState::Freed {
                self.stale += 1;
            }
            self.head += front.span;
            self.blocks.pop_front();
            self.first_seq += 1;
        }

        let entries: usize = self.free.iter().map(Vec::len).sum();
        if self.stale * 2 > entries {
            let first = self.first_seq;
            for list in &mut self.free {
                list.retain(|&seq| seq >= first);
            }
            self.stale = 0;
        }
    }

    /// Number of entries on the free lists, stale ones included.
    #[cfg(test)]
    pub(super) fn free_entries(&self) -> usize {
        self.free.iter().map(Vec::len).sum()
    }
}
