//! The circular buffer's metadata, kept behind its mutex.
//!
//! Per-block metadata lives inside each block's 8-byte slab header, read and
//! written only while `Mutex<State>` is held. No `Block` slice covers header
//! bytes.

#![allow(unsafe_code)]

use bf_tree_node_id::LeafId;

use crate::MAX_CLASSES;

use super::slab::Slab;
use super::{BLOCK_HEADER_BYTES, Kind, Metrics};

/// Sentinel `owner` value for a wrap filler block.
const NO_OWNER: u32 = u32::MAX;

const KIND_BIT: u8 = 1 << 0;
const FREED_WHILE_CLAIMED_BIT: u8 = 1 << 1;
const CLASS_SHIFT: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum BlockState {
    /// Held by its owner through a `Block`.
    Live = 0,
    /// Freed outside the second-chance region; on a free list for reuse.
    Freed = 1,
    /// Freed, never reused; dead until the head passes it.
    Tombstone = 2,
    /// Claimed by an evictor. Never reused; the head waits for it.
    Evicting = 3,
    /// Retired by its evictor after the owner returned the handle.
    Evicted = 4,
    /// Covers the end of the slab when a block would cross it; never had a
    /// handle.
    Filler = 5,
}

impl BlockState {
    fn from_byte(byte: u8) -> Self {
        match byte {
            0 => Self::Live,
            1 => Self::Freed,
            2 => Self::Tombstone,
            3 => Self::Evicting,
            4 => Self::Evicted,
            5 => Self::Filler,
            _ => panic!("corrupt circular-buffer block state {byte}"),
        }
    }

    /// Whether the head may pass the block.
    fn is_dead(self) -> bool {
        match self {
            Self::Live | Self::Evicting => false,
            Self::Freed | Self::Tombstone | Self::Evicted | Self::Filler => true,
        }
    }
}

/// Eight bytes at the start of every block in the slab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct BlockHeader {
    owner: u32,
    /// Block span (header plus payload, or filler bytes) in 8-byte units.
    span_units: u16,
    pub(super) state: BlockState,
    /// Bit 0: `Kind`; bit 1: `freed_while_claimed`; bits 2..8: class index.
    flags: u8,
}

impl BlockHeader {
    pub(super) fn new(
        span: usize,
        state: BlockState,
        kind: Kind,
        class: usize,
        owner: Option<u32>,
    ) -> Self {
        debug_assert!(span >= BLOCK_HEADER_BYTES && span.is_multiple_of(BLOCK_HEADER_BYTES));
        debug_assert!(class < MAX_CLASSES);

        let span_units = u16::try_from(span / BLOCK_HEADER_BYTES).expect("validated block span");

        let mut flags = (class as u8) << CLASS_SHIFT;
        if kind == Kind::Full {
            flags |= KIND_BIT;
        }

        Self {
            owner: owner.unwrap_or(NO_OWNER),
            span_units,
            state,
            flags,
        }
    }

    pub(super) fn span(self) -> usize {
        usize::from(self.span_units) * BLOCK_HEADER_BYTES
    }

    pub(super) fn kind(self) -> Kind {
        if self.flags & KIND_BIT != 0 {
            Kind::Full
        } else {
            Kind::Mini
        }
    }

    pub(super) fn set_kind(&mut self, kind: Kind) {
        if kind == Kind::Full {
            self.flags |= KIND_BIT;
        } else {
            self.flags &= !KIND_BIT;
        }
    }

    pub(super) fn freed_while_claimed(self) -> bool {
        self.flags & FREED_WHILE_CLAIMED_BIT != 0
    }

    pub(super) fn set_freed_while_claimed(&mut self, value: bool) {
        if value {
            self.flags |= FREED_WHILE_CLAIMED_BIT;
        } else {
            self.flags &= !FREED_WHILE_CLAIMED_BIT;
        }
    }

    pub(super) fn owner(self) -> Option<LeafId> {
        if self.owner == NO_OWNER {
            None
        } else {
            Some(LeafId::new(u64::from(self.owner)).expect("u32 fits LeafId"))
        }
    }

    pub(super) fn set_owner(&mut self, owner: u32) {
        self.owner = owner;
    }

    fn to_bytes(self) -> [u8; BLOCK_HEADER_BYTES] {
        let mut bytes = [0u8; BLOCK_HEADER_BYTES];


        bytes[0..4].copy_from_slice(&self.owner.to_le_bytes());
        bytes[4..6].copy_from_slice(&self.span_units.to_le_bytes());
        bytes[6] = self.state as u8;
        bytes[7] = self.flags;


        bytes
    }

    fn from_bytes(bytes: [u8; BLOCK_HEADER_BYTES]) -> Self {
        let owner = u32::from_le_bytes(bytes[0..4].try_into().expect("4 bytes"));
        let span_units = u16::from_le_bytes(bytes[4..6].try_into().expect("2 bytes"));
        let state = BlockState::from_byte(bytes[6]);
        let flags = bytes[7];


        Self {
            owner,
            span_units,
            state,
            flags,
        }
    }
}

#[derive(Debug)]
pub(super) struct State {
    pub(super) head: usize,
    pub(super) tail: usize,
    /// Handles not yet returned (live blocks, and claimed blocks whose owner
    /// has not freed them). The slab is leaked on drop unless this is 0.
    pub(super) outstanding: usize,
    /// Per size class: logical addresses of `Freed` blocks. An entry whose
    /// block the head has passed is stale (`addr < head`).
    free: Vec<Vec<usize>>,
    /// Number of stale entries in `free`.
    stale: usize,
    pub(super) metrics: Metrics,
}

impl State {
    pub(super) fn new(classes: usize) -> Self {
        Self {
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

    /// Whether `addr` is the block at the head.
    pub(super) fn is_head(&self, addr: usize) -> bool {
        addr == self.head
    }

    pub(super) fn used(&self) -> usize {
        self.tail - self.head
    }

    /// Whether `addr` lies in `[head, tail)` on a header boundary.
    pub(super) fn in_active_window(&self, addr: usize) -> bool {
        addr >= self.head && addr < self.tail
    }

    /// Read the header of an active block at `addr`.
    pub(super) fn read_header(&self, slab: &Slab, addr: usize) -> BlockHeader {
        debug_assert!(self.in_active_window(addr));


        let offset = addr & (slab.capacity() - 1);

        // SAFETY: `Mutex<State>` serializes all header access, and no `Block`
        // slice ever covers the 8-byte header at `offset`.
        let bytes: [u8; BLOCK_HEADER_BYTES] = unsafe { slab.read_bytes(offset) };


        BlockHeader::from_bytes(bytes)
    }

    /// Write the header of a block at `addr` (either active or being bumped at
    /// `tail`).
    pub(super) fn write_header(&mut self, slab: &Slab, addr: usize, header: BlockHeader) {
        let offset = addr & (slab.capacity() - 1);

        // SAFETY: `Mutex<State>` serializes all header access, and no `Block`
        // slice ever covers the 8-byte header at `offset`.
        unsafe { slab.write_bytes(offset, header.to_bytes()) };
    }

    /// Make the live block at `addr` a hole on the free list of `class`.
    pub(super) fn add_hole(&mut self, slab: &Slab, addr: usize, class: usize) {
        let mut header = self.read_header(slab, addr);
        header.state = BlockState::Freed;
        self.write_header(slab, addr, header);
        self.free[class].push(addr);
    }

    /// Take a hole of `class` that lies outside the second-chance region and
    /// make it live for `kind` and `owner`. Holes that have aged into the
    /// region become tombstones on the way. Returns the hole's logical address.
    pub(super) fn take_hole(
        &mut self,
        slab: &Slab,
        class: usize,
        threshold: usize,
        kind: Kind,
        owner: u32,
    ) -> Option<usize> {
        while let Some(addr) = self.free[class].pop() {
            if addr < self.head {
                // The head passed this hole; its space is already free.
                self.stale -= 1;
                continue;
            }
            let mut header = self.read_header(slab, addr);
            debug_assert_eq!(header.state, BlockState::Freed);

            if self.in_second_chance(addr, threshold) {
                header.state = BlockState::Tombstone;
                self.write_header(slab, addr, header);
                self.metrics.tombstones += 1;
                continue;
            }

            header.state = BlockState::Live;
            header.set_kind(kind);
            header.set_owner(owner);
            header.set_freed_while_claimed(false);
            self.write_header(slab, addr, header);
            return Some(addr);
        }
        None
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

    /// Reserve `span` bytes at the tail for `owner`, after a filler if
    /// needed, and return the new block's logical address. `None` if the
    /// budget does not allow it; nothing changes then.
    pub(super) fn bump(
        &mut self,
        slab: &Slab,
        span: usize,
        kind: Kind,
        class: usize,
        owner: u32,
    ) -> Option<usize> {
        let capacity = slab.capacity();
        if !self.fits(capacity, span) {
            return None;
        }

        let filler = self.filler(capacity, span);
        if filler > 0 {
            let addr = self.tail;
            let header = BlockHeader::new(filler, BlockState::Filler, Kind::Mini, 0, None);
            self.write_header(slab, addr, header);
            self.tail += filler;
            self.metrics.fillers += 1;
        }
        let addr = self.tail;
        let header = BlockHeader::new(span, BlockState::Live, kind, class, Some(owner));
        self.write_header(slab, addr, header);
        self.tail += span;

        Some(addr)
    }

    /// The oldest live block, marked `Evicting`, with its logical address and
    /// header. `Err(true)` if every remaining block that is not dead is
    /// already claimed, `Err(false)` if none is.
    pub(super) fn claim_oldest(&mut self, slab: &Slab) -> Result<(usize, BlockHeader), bool> {
        let mut addr = self.head;
        let mut busy = false;
        while addr < self.tail {
            let mut header = self.read_header(slab, addr);
            match header.state {
                BlockState::Live => {
                    header.state = BlockState::Evicting;
                    self.write_header(slab, addr, header);
                    return Ok((addr, header));
                }
                BlockState::Evicting => busy = true,
                _ => {}
            }
            addr += header.span();
        }
        Err(busy)
    }

    /// Move the head past every dead block at the front. Holes it passes
    /// leave stale free-list entries; when those exceed half of all entries,
    /// every list is filtered, so the cost is amortized over the frees that
    /// created them.
    pub(super) fn advance_head(&mut self, slab: &Slab) {
        while self.head < self.tail {
            let header = self.read_header(slab, self.head);
            if !header.state.is_dead() {
                break;
            }

            if header.state == BlockState::Freed {
                self.stale += 1;
            }

            self.head += header.span();
        }

        let entries: usize = self.free.iter().map(Vec::len).sum();
        if self.stale * 2 > entries {
            let head = self.head;
            for list in &mut self.free {
                list.retain(|&addr| addr >= head);
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
