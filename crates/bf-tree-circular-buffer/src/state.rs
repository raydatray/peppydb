//! The circular buffer's metadata, kept behind its mutex. Safe code only.

use std::collections::VecDeque;

use super::Metrics;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BlockState {
    /// Held by its owner through a `Block`.
    Live,
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
            Self::Tombstone | Self::Filler => true,
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

#[derive(Debug, Default)]
pub(super) struct State {
    /// Blocks from head to tail in allocation order; index = seq - first_seq.
    blocks: VecDeque<BlockMeta>,
    first_seq: u64,
    pub(super) head: usize,
    pub(super) tail: usize,
    /// Live blocks. The slab is leaked on drop unless this is 0.
    pub(super) outstanding: usize,
    pub(super) metrics: Metrics,
}

impl State {
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

    /// Move the head past every dead block at the front.
    pub(super) fn advance_head(&mut self) {
        while let Some(front) = self.blocks.front()
            && front.state.is_dead()
        {
            self.head += front.span;
            self.blocks.pop_front();
            self.first_seq += 1;
        }
    }
}
