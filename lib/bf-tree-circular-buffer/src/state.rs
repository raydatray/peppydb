//! The circular buffer's metadata, kept behind its mutex.
//!
//! Per-block metadata lives inside each block's 8-byte slab header, and
//! per-class doubly-linked free-list pointers live in the first 16 payload
//! bytes of `Freed` blocks. Both are read and written only while
//! `Mutex<State>` is held: no `Block` slice covers header bytes, and no
//! `Block` handle exists for a `Freed` block.

#![allow(unsafe_code)]

use bf_tree_node_id::LeafId;

use super::slab::Slab;
use super::{BLOCK_HEADER_BYTES, Claim, Kind, Metrics, Victim};

/// Sentinel `owner` value for a wrap filler block.
const NO_OWNER: u32 = u32::MAX;

/// Sentinel logical address for an empty free-list link.
const NONE_ADDR: u64 = u64::MAX;

/// Bytes borrowed from a `Freed` block's payload for `(prev, next)`.
const LINK_BYTES: usize = 16;

const KIND_BIT: u8 = 1 << 0;
const FREED_WHILE_CLAIMED_BIT: u8 = 1 << 1;
const CLASS_SHIFT: u32 = 2;

/// Maximum number of size classes that fit in the remaining bits of `flags`.
pub(super) const MAX_CLASSES: usize = 1 << (u8::BITS - CLASS_SHIFT);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum BlockState {
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
struct BlockHeader {
    owner: u32,
    /// Block span (header plus payload, or filler bytes) in 8-byte units.
    span_units: u16,
    state: BlockState,
    /// Bit 0: `Kind`; bit 1: `freed_while_claimed`; bits 2..8: class index.
    flags: u8,
}

impl BlockHeader {
    fn new(span: usize, state: BlockState, kind: Kind, class: usize, owner: Option<u32>) -> Self {
        debug_assert!(span >= BLOCK_HEADER_BYTES && span.is_multiple_of(BLOCK_HEADER_BYTES));
        let span_units = u16::try_from(span / BLOCK_HEADER_BYTES).expect("validated block span");
        debug_assert!(class < MAX_CLASSES);
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

    fn span(self) -> usize {
        usize::from(self.span_units) * BLOCK_HEADER_BYTES
    }

    fn class(self) -> usize {
        usize::from(self.flags >> CLASS_SHIFT)
    }

    fn kind(self) -> Kind {
        if self.flags & KIND_BIT != 0 {
            Kind::Full
        } else {
            Kind::Mini
        }
    }

    fn set_kind(&mut self, kind: Kind) {
        if kind == Kind::Full {
            self.flags |= KIND_BIT;
        } else {
            self.flags &= !KIND_BIT;
        }
    }

    fn freed_while_claimed(self) -> bool {
        self.flags & FREED_WHILE_CLAIMED_BIT != 0
    }

    fn set_freed_while_claimed(&mut self, value: bool) {
        if value {
            self.flags |= FREED_WHILE_CLAIMED_BIT;
        } else {
            self.flags &= !FREED_WHILE_CLAIMED_BIT;
        }
    }

    fn owner(self) -> Option<LeafId> {
        if self.owner == NO_OWNER {
            None
        } else {
            Some(LeafId::new(u64::from(self.owner)).expect("u32 fits LeafId"))
        }
    }

    fn set_owner(&mut self, owner: u32) {
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
    /// Per size class: logical address of the newest `Freed` hole, or
    /// `NONE_ADDR`. Doubly-linked through each hole's dead payload so
    /// `advance_head` unlinks passed holes in O(1) with zero stale entries.
    free_head: Box<[u64]>,
    pub(super) metrics: Metrics,
}

impl State {
    pub(super) fn new(classes: usize) -> Self {
        Self {
            head: 0,
            tail: 0,
            outstanding: 0,
            free_head: vec![NONE_ADDR; classes].into_boxed_slice(),
            metrics: Metrics::default(),
        }
    }

    /// Whether the block at `addr` is in the second-chance region: at least
    /// `threshold` bytes behind the tail.
    fn in_second_chance(&self, addr: usize, threshold: usize) -> bool {
        self.tail.saturating_sub(addr) >= threshold
    }

    /// Whether `addr` is the block at the head.
    fn is_head(&self, addr: usize) -> bool {
        addr == self.head
    }

    pub(super) fn used(&self) -> usize {
        self.tail - self.head
    }

    /// Whether `addr` lies in the active `[head, tail)` range.
    fn in_active_window(&self, addr: usize) -> bool {
        addr >= self.head && addr < self.tail
    }

    /// Read the header of an active block at `addr`.
    ///
    /// # Safety
    /// `slab` must belong to this `State` (guarded by `Mutex<State>`), and
    /// `addr` must be the logical start of an active block in `[head, tail)`.
    unsafe fn read_header(&self, slab: &Slab, addr: usize) -> BlockHeader {
        debug_assert!(self.in_active_window(addr));

        let offset = addr & (slab.capacity() - 1);

        // SAFETY: the caller guarantees `slab` belongs to this `Mutex<State>`
        // and `addr` is an active block header, which no `Block` slice covers.
        let bytes: [u8; BLOCK_HEADER_BYTES] = unsafe { slab.read_bytes(offset) };

        BlockHeader::from_bytes(bytes)
    }

    /// Write the header of a block at `addr` (either active or being bumped at
    /// `tail`).
    ///
    /// # Safety
    /// `slab` must belong to this `State` (guarded by `Mutex<State>`), and
    /// `addr` must be the logical start of an active block or the newly
    /// reserved range at `tail`.
    unsafe fn write_header(&mut self, slab: &Slab, addr: usize, header: BlockHeader) {
        let offset = addr & (slab.capacity() - 1);

        // SAFETY: the caller guarantees `slab` belongs to this `Mutex<State>`
        // and `addr` is a block header, which no `Block` slice covers.
        unsafe { slab.write_bytes(offset, header.to_bytes()) };
    }

    /// Read `(prev, next)` links from the dead payload of a `Freed` block.
    ///
    /// # Safety
    /// `slab` must belong to this `State` (guarded by `Mutex<State>`), and
    /// `addr` must be an active block in `BlockState::Freed` (so no `Block`
    /// handle exists for its payload).
    unsafe fn read_links(&self, slab: &Slab, addr: usize) -> (u64, u64) {
        debug_assert_eq!(
            // SAFETY: caller guarantees `addr` is an active block in `slab`.
            unsafe { self.read_header(slab, addr) }.state,
            BlockState::Freed
        );

        let payload_offset = (addr & (slab.capacity() - 1)) + BLOCK_HEADER_BYTES;

        // SAFETY: the caller guarantees `addr` is `Freed` under `Mutex<State>`,
        // so its `Block` handle was consumed and no live reference covers its payload.
        let bytes: [u8; LINK_BYTES] = unsafe { slab.read_bytes(payload_offset) };
        let prev = u64::from_le_bytes(bytes[0..8].try_into().expect("8 bytes"));
        let next = u64::from_le_bytes(bytes[8..16].try_into().expect("8 bytes"));

        (prev, next)
    }

    /// Write `(prev, next)` links into the dead payload of a `Freed` block.
    ///
    /// # Safety
    /// `slab` must belong to this `State` (guarded by `Mutex<State>`), and
    /// `addr` must be an active block in `BlockState::Freed` (so no `Block`
    /// handle exists for its payload).
    unsafe fn write_links(&mut self, slab: &Slab, addr: usize, prev: u64, next: u64) {
        debug_assert_eq!(
            // SAFETY: caller guarantees `addr` is an active block in `slab`.
            unsafe { self.read_header(slab, addr) }.state,
            BlockState::Freed
        );

        let payload_offset = (addr & (slab.capacity() - 1)) + BLOCK_HEADER_BYTES;

        let mut bytes = [0u8; LINK_BYTES];
        bytes[0..8].copy_from_slice(&prev.to_le_bytes());
        bytes[8..16].copy_from_slice(&next.to_le_bytes());

        // SAFETY: the caller guarantees `addr` is `Freed` under `Mutex<State>`,
        // so its `Block` handle was consumed and no live reference covers its payload.
        unsafe { slab.write_bytes(payload_offset, bytes) };
    }

    /// Make the live block at `addr` a hole on the free list of `class`.
    ///
    /// # Safety
    /// `addr` must be an active block of `slab` whose `Block` handle was just
    /// consumed.
    unsafe fn add_hole(&mut self, slab: &Slab, addr: usize, class: usize) {
        // SAFETY: caller guarantees `addr` is an active block of `slab`.
        let mut header = unsafe { self.read_header(slab, addr) };
        header.state = BlockState::Freed;
        // SAFETY: same active block header.
        unsafe { self.write_header(slab, addr, header) };

        let old = self.free_head[class];
        let addr_u64 = addr as u64;

        // SAFETY: `addr` was just marked `Freed` after its `Block` was consumed.
        unsafe { self.write_links(slab, addr, NONE_ADDR, old) };

        if old != NONE_ADDR {
            // SAFETY: every entry on `free_head[class]` is an active `Freed` block.
            let (old_prev, old_next) = unsafe { self.read_links(slab, old as usize) };
            debug_assert_eq!(old_prev, NONE_ADDR);
            // SAFETY: `old` is an active `Freed` block.
            unsafe { self.write_links(slab, old as usize, addr_u64, old_next) };
        }

        self.free_head[class] = addr_u64;
    }

    /// Unlink a `Freed` block at `addr` from `free_head[class]` in O(1).
    ///
    /// # Safety
    /// `addr` must be an active `Freed` block currently linked on
    /// `free_head[class]`.
    unsafe fn unlink_hole(&mut self, slab: &Slab, addr: usize, class: usize) {
        // SAFETY: caller guarantees `addr` is an active `Freed` block.
        let (prev, next) = unsafe { self.read_links(slab, addr) };

        if prev == NONE_ADDR {
            debug_assert_eq!(self.free_head[class], addr as u64);

            self.free_head[class] = next;
        } else {
            // SAFETY: every neighbour in a free list is an active `Freed` block.
            let (prev_prev, prev_next) = unsafe { self.read_links(slab, prev as usize) };
            debug_assert_eq!(prev_next, addr as u64);

            // SAFETY: `prev` is an active `Freed` block.
            unsafe { self.write_links(slab, prev as usize, prev_prev, next) };
        }

        if next != NONE_ADDR {
            // SAFETY: every neighbour in a free list is an active `Freed` block.
            let (next_prev, next_next) = unsafe { self.read_links(slab, next as usize) };
            debug_assert_eq!(next_prev, addr as u64);

            // SAFETY: `next` is an active `Freed` block.
            unsafe { self.write_links(slab, next as usize, prev, next_next) };
        }

        // SAFETY: `addr` is still in `BlockState::Freed`.
        unsafe { self.write_links(slab, addr, NONE_ADDR, NONE_ADDR) };
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
        while self.free_head[class] != NONE_ADDR {
            let addr = self.free_head[class] as usize;

            // SAFETY: `free_head[class]` holds only active `Freed` blocks.
            unsafe { self.unlink_hole(slab, addr, class) };

            // SAFETY: `addr` is an active block of `slab`.
            let mut header = unsafe { self.read_header(slab, addr) };
            debug_assert_eq!(header.state, BlockState::Freed);

            if self.in_second_chance(addr, threshold) {
                header.state = BlockState::Tombstone;

                // SAFETY: `addr` is an active block of `slab`.
                unsafe { self.write_header(slab, addr, header) };

                self.metrics.tombstones += 1;
                continue;
            }

            header.state = BlockState::Live;
            header.set_kind(kind);
            header.set_owner(owner);
            header.set_freed_while_claimed(false);

            // SAFETY: `addr` is an active block of `slab`.
            unsafe { self.write_header(slab, addr, header) };

            return Some(addr);
        }

        None
    }

    /// Return a block that `CircularBuffer::free` already verified belongs to
    /// `slab` at `(addr, span)`.
    pub(super) fn free_block(
        &mut self,
        slab: &Slab,
        addr: usize,
        span: usize,
        class: usize,
        second_chance: usize,
    ) {
        let header = self
            .in_active_window(addr)
            // SAFETY: `addr` came from a verified `Block` of this buffer.
            .then(|| unsafe { self.read_header(slab, addr) })
            .filter(|header| header.span() == span);

        let mut header = match header {
            Some(header)
                if header.state == BlockState::Live
                    || (header.state == BlockState::Evicting && !header.freed_while_claimed()) =>
            {
                header
            }
            _ => panic!("block is not a live block of this circular buffer"),
        };

        if header.state == BlockState::Evicting {
            // The evictor's `retire` makes it dead.
            header.set_freed_while_claimed(true);

            // SAFETY: `addr` is a verified active block of `slab`.
            unsafe { self.write_header(slab, addr, header) };
        } else if self.is_head(addr) || self.in_second_chance(addr, second_chance) {
            header.state = BlockState::Tombstone;

            // SAFETY: `addr` is a verified active block of `slab`.
            unsafe { self.write_header(slab, addr, header) };

            self.metrics.tombstones += 1;
        } else {
            // SAFETY: `addr` is a verified active block whose `Block` was consumed.
            unsafe { self.add_hole(slab, addr, class) };
        }

        self.outstanding -= 1;
        self.metrics.frees += 1;
        self.advance_head(slab);
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

            // SAFETY: `fits` ensured `[addr, addr + filler)` is free space at `tail`.
            unsafe { self.write_header(slab, addr, header) };

            self.tail += filler;
            self.metrics.fillers += 1;
        }

        let addr = self.tail;
        let header = BlockHeader::new(span, BlockState::Live, kind, class, Some(owner));

        // SAFETY: `fits` ensured `[addr, addr + span)` is free space at `tail`.
        unsafe { self.write_header(slab, addr, header) };

        self.tail += span;

        Some(addr)
    }

    /// Claim the oldest live block by marking it `Evicting`. Returns
    /// [`Claim::Busy`] if every remaining live block is already claimed, or
    /// [`Claim::Empty`] if none is.
    pub(super) fn claim_oldest(&mut self, slab: &Slab) -> Claim {
        let mut addr = self.head;
        let mut busy = false;

        while addr < self.tail {
            // SAFETY: walking by `span` from `head` visits only active blocks.
            let mut header = unsafe { self.read_header(slab, addr) };
            match header.state {
                BlockState::Live => {
                    header.state = BlockState::Evicting;

                    // SAFETY: `addr` is an active block of `slab`.
                    unsafe { self.write_header(slab, addr, header) };

                    let owner = header.owner().expect("live blocks have an owner");
                    return Claim::Victim(Victim::new(addr, header.kind(), owner));
                }
                BlockState::Evicting => busy = true,
                _ => {}
            }
            addr += header.span();
        }

        if busy { Claim::Busy } else { Claim::Empty }
    }

    /// Finish an eviction for a verified `victim` of `slab`.
    pub(super) fn retire(&mut self, slab: &Slab, victim: &Victim) {
        let header = self.claimed_header(slab, victim);
        assert!(
            header.freed_while_claimed(),
            "retire requires the block to be freed first"
        );

        self.finish_eviction(slab, victim.addr(), header);
    }

    /// Give a claim back for a verified `victim` of `slab`.
    pub(super) fn unclaim(&mut self, slab: &Slab, victim: &Victim) {
        let mut header = self.claimed_header(slab, victim);
        if header.freed_while_claimed() {
            self.finish_eviction(slab, victim.addr(), header);
        } else {
            header.state = BlockState::Live;

            // SAFETY: `claimed_header` verified `victim.addr()` is an active block.
            unsafe { self.write_header(slab, victim.addr(), header) };
        }
    }

    /// Check that `victim` (already verified to belong to `slab`) names a
    /// currently claimed block, and return its header.
    fn claimed_header(&self, slab: &Slab, victim: &Victim) -> BlockHeader {
        let addr = victim.addr();
        let header = self
            .in_active_window(addr)
            // SAFETY: `victim` came from this buffer's `claim_victim`, so its
            // address is a block start; `in_active_window` ensures it is active.
            .then(|| unsafe { self.read_header(slab, addr) });

        match header {
            Some(header)
                if header.state == BlockState::Evicting
                    && header.owner() == Some(victim.owner)
                    && header.kind() == victim.kind =>
            {
                header
            }
            _ => panic!("victim is not a claimed block of this circular buffer"),
        }
    }

    /// Mark a claimed, freed block evicted and move the head past it if it can.
    fn finish_eviction(&mut self, slab: &Slab, addr: usize, mut header: BlockHeader) {
        header.state = BlockState::Evicted;

        // SAFETY: `claimed_header` verified `addr` is an active block of `slab`.
        unsafe { self.write_header(slab, addr, header) };

        self.metrics.retired += 1;
        self.advance_head(slab);
    }

    /// Move the head past every dead block at the front, unlinking any `Freed`
    /// hole from its class's free list in O(1).
    pub(super) fn advance_head(&mut self, slab: &Slab) {
        while self.head < self.tail {
            // SAFETY: `self.head` is always the start of the oldest active block.
            let header = unsafe { self.read_header(slab, self.head) };

            if !header.state.is_dead() {
                break;
            }

            if header.state == BlockState::Freed {
                // SAFETY: `self.head` is an active `Freed` block on its class list.
                unsafe { self.unlink_hole(slab, self.head, header.class()) };
            }

            self.head += header.span();
        }
    }

    /// Number of entries across all free lists, checking doubly-linked
    /// integrity along the way.
    #[cfg(test)]
    pub(super) fn free_entries(&self, slab: &Slab) -> usize {
        let mut count = 0;
        for (class, &head) in self.free_head.iter().enumerate() {
            let mut prev = NONE_ADDR;
            let mut cur = head;
            while cur != NONE_ADDR {
                let addr = cur as usize;
                assert!(self.in_active_window(addr), "stale free-list entry");
                // SAFETY: every entry on `free_head[class]` is an active block.
                let header = unsafe { self.read_header(slab, addr) };
                assert_eq!(header.state, BlockState::Freed);
                assert_eq!(header.class(), class);
                // SAFETY: checked `BlockState::Freed` above.
                let (got_prev, next) = unsafe { self.read_links(slab, addr) };
                assert_eq!(got_prev, prev);
                prev = cur;
                cur = next;
                count += 1;
            }
        }
        count
    }
}
