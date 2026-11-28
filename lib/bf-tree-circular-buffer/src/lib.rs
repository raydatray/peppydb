//! A bounded circular buffer of variable-size blocks (BF-Tree paper, §4).
//!
//! Every block lives in one fixed allocation, the slab. Blocks are allocated
//! at the tail; the head moves forward over dead blocks, and the bytes between
//! head and tail are the budget in use. A block never crosses the end of the
//! slab: when it would, a dead filler block covers the rest of the slab first.
//!
//! A freed block becomes a hole that a later allocation of the same size class
//! reuses in place, unless it lies in the **second-chance region**: the oldest
//! part of the buffer, near the head. Blocks freed there become tombstones,
//! and an evictor can copy a hot block out of it instead of evicting it.
//!
//! A [`Block`] is the only handle to its bytes. Metadata lives behind one
//! mutex, which is never held while block bytes are read, written, or zeroed.
//!
//! # Eviction
//!
//! An evictor calls [`CircularBuffer::claim_victim`] with no lock held. The
//! claimed block is never reused and the head waits for it. The evictor then
//! locks the owner and checks whether its entry still holds the block
//! ([`Block::is`]). If it does, the evictor writes the contents back, takes
//! the block out, returns it with [`CircularBuffer::free`], and calls
//! [`CircularBuffer::retire`]. If it does not, the owner already freed the
//! block, and the evictor only retires it. A [`Victim`] gives no access to
//! bytes.
//!
//! ```
//! use bf_tree_circular_buffer::{BLOCK_HEADER_BYTES, CircularBuffer, Kind};
//! use bf_tree_node_id::LeafId;
//!
//! let buffer = CircularBuffer::new(1024, &[64, 128], 0).unwrap();
//! let mut block = buffer.alloc(64, Kind::Mini, LeafId::new(7).unwrap()).unwrap();
//! block.as_mut()[0] = 42;
//! assert_eq!(block.as_ref().len(), 64);
//! assert_eq!(buffer.used(), BLOCK_HEADER_BYTES + 64);
//!
//! buffer.free(block);
//! assert_eq!(buffer.used(), 0);
//! ```
//!
//! # Locking
//!
//! The metadata mutex comes from `bf-tree-sync`, which ignores poisoning. Every
//! critical section here checks before it mutates, as that crate requires.

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::sync::atomic::{AtomicUsize, Ordering};

use bf_tree_sync::{Mutex, MutexGuard};

use bf_tree_node_id::LeafId;

mod block;
mod slab;
mod state;

pub use block::Block;
use slab::Slab;
use state::{MAX_CLASSES, State};

/// Bookkeeping bytes charged to every block on top of its size.
pub const BLOCK_HEADER_BYTES: usize = 8;

/// Minimum payload size: a freed block's payload holds two 8-byte links.
const MIN_CLASS_BYTES: usize = 16;

/// What a block holds; recorded for eviction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Mini,
    Full,
}

/// A block claimed for eviction. It names the block; it gives no access to
/// its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Victim {
    /// The block's logical address.
    addr: usize,
    pub kind: Kind,
    /// The leaf the block was allocated for.
    pub owner: LeafId,
}

impl Victim {
    fn new(addr: usize, kind: Kind, owner: LeafId) -> Self {
        Self { addr, kind, owner }
    }

    /// The block's logical address in its circular buffer.
    pub fn addr(&self) -> usize {
        self.addr
    }
}

/// The outcome of [`CircularBuffer::claim_victim`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    /// The oldest live block, now claimed by the caller.
    Victim(Victim),
    /// Every live block is claimed by another evictor.
    Busy,
    /// The buffer holds no live block.
    Empty,
}

/// The circular buffer has no room for the allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("circular buffer is full")]
pub struct CircularBufferFull;

/// A rejected circular-buffer configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CircularBufferError {
    #[error("invalid capacity: {0}")]
    InvalidCapacity(&'static str),
    #[error("invalid size classes: {0}")]
    InvalidClasses(&'static str),
    #[error("second-chance percent {0} exceeds 100")]
    InvalidPercent(u32),
}

/// Operation counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Metrics {
    /// Blocks allocated at the tail.
    pub allocs: u64,
    /// Blocks allocated by reusing a hole.
    pub reuses: u64,
    /// Blocks returned with `free`.
    pub frees: u64,
    /// Blocks that became tombstones.
    pub tombstones: u64,
    /// Claimed blocks retired by an evictor.
    pub retired: u64,
    /// Filler blocks placed at the end of the slab.
    pub fillers: u64,
}

/// A bounded circular buffer of blocks.
#[derive(Debug)]
pub struct CircularBuffer {
    slab: Slab,
    state: Mutex<State>,
    /// Copy of the state's tail for `in_second_chance` without the mutex.
    tail: AtomicUsize,
    capacity: usize,
    /// Allocation sizes, strictly ascending.
    classes: Box<[usize]>,
    /// A block at least this far behind the tail is in the second-chance
    /// region; `usize::MAX` when the region is empty.
    second_chance: usize,
}

// Compile-time check: the buffer and its blocks can be shared between threads.
const _: fn() = || {
    fn check<T: Send + Sync>() {}
    check::<CircularBuffer>();
    check::<Block>();
};

impl CircularBuffer {
    /// Check configuration parameters without allocating a slab.
    pub fn validate(
        capacity: usize,
        classes: &[usize],
        second_chance_percent: u32,
    ) -> Result<(), CircularBufferError> {
        let (Some(&smallest), Some(&largest)) = (classes.first(), classes.last()) else {
            return Err(CircularBufferError::InvalidClasses("no classes"));
        };
        if classes.len() > MAX_CLASSES {
            return Err(CircularBufferError::InvalidClasses("more than 64 classes"));
        }
        if smallest < MIN_CLASS_BYTES {
            return Err(CircularBufferError::InvalidClasses(
                "a class is below 16 bytes",
            ));
        }
        if classes.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(CircularBufferError::InvalidClasses(
                "classes are not strictly ascending",
            ));
        }
        if classes
            .iter()
            .any(|&class| !class.is_multiple_of(BLOCK_HEADER_BYTES))
        {
            return Err(CircularBufferError::InvalidClasses(
                "classes must be multiples of 8 bytes",
            ));
        }
        let largest_span = largest.checked_add(BLOCK_HEADER_BYTES);
        if largest_span.is_none_or(|span| span / BLOCK_HEADER_BYTES > usize::from(u16::MAX)) {
            return Err(CircularBufferError::InvalidClasses(
                "a class exceeds the maximum block span",
            ));
        }
        if !capacity.is_power_of_two() {
            return Err(CircularBufferError::InvalidCapacity("not a power of two"));
        }
        let minimum = largest_span.and_then(|span| span.checked_mul(2));
        if minimum.is_none_or(|minimum| capacity < minimum) {
            return Err(CircularBufferError::InvalidCapacity(
                "less than twice the largest class plus its header",
            ));
        }
        if capacity > isize::MAX as usize {
            return Err(CircularBufferError::InvalidCapacity("exceeds isize::MAX"));
        }
        if second_chance_percent > 100 {
            return Err(CircularBufferError::InvalidPercent(second_chance_percent));
        }
        Ok(())
    }

    /// A circular buffer of `capacity` bytes serving blocks of the sizes in
    /// `classes`.
    ///
    /// `capacity` must be a power of two of at least twice the largest class
    /// plus its 8-byte header, so that an empty buffer always fits the
    /// largest block even after a filler. `classes` must be nonempty, strictly
    /// ascending multiples of 8 bytes (at least 16, at most 64 classes).
    /// `second_chance_percent` is the share of the buffer, at the head, from
    /// which freed blocks are not reused; it must be at most 100.
    pub fn new(
        capacity: usize,
        classes: &[usize],
        second_chance_percent: u32,
    ) -> Result<Self, CircularBufferError> {
        Self::validate(capacity, classes, second_chance_percent)?;

        // capacity * (100 - p) / 100, exactly; p = 0 means no region.
        let second_chance = match second_chance_percent {
            0 => usize::MAX,
            p => {
                let share = capacity as u128 * u128::from(100 - p) / 100;
                usize::try_from(share).expect("at most capacity")
            }
        };

        Ok(Self {
            slab: Slab::new(capacity),
            state: Mutex::new(State::new(classes.len())),
            tail: AtomicUsize::new(0),
            capacity,
            classes: classes.into(),
            second_chance,
        })
    }

    /// Allocate a zeroed block of exactly `size` bytes for `owner`. Panics if
    /// `size` is not one of the classes.
    ///
    /// A hole of the same class outside the second-chance region is reused
    /// first, as upstream does; holes of larger classes are not used. Otherwise
    /// the block is placed at the tail.
    pub fn alloc(
        &self,
        size: usize,
        kind: Kind,
        owner: LeafId,
    ) -> Result<Block, CircularBufferFull> {
        let class = self.class(size);
        let len = u32::try_from(size).expect("classes fit in u32");
        let owner_raw = u32::try_from(owner.raw())
            .ok()
            .filter(|&raw| raw != u32::MAX)
            .expect("owner leaf ID fits in 32 bits");

        let addr = {
            let mut state = self.lock_state();
            let reserved =
                match state.take_hole(&self.slab, class, self.second_chance, kind, owner_raw) {
                    Some(hole) => {
                        state.metrics.reuses += 1;
                        hole
                    }
                    None => {
                        let bumped = state
                            .bump(
                                &self.slab,
                                BLOCK_HEADER_BYTES + size,
                                kind,
                                class,
                                owner_raw,
                            )
                            .ok_or(CircularBufferFull)?;
                        state.metrics.allocs += 1;
                        self.tail.store(state.tail, Ordering::Relaxed);
                        bumped
                    }
                };
            state.outstanding += 1;
            reserved
        };

        // Zero outside the mutex: the reservation made this the only handle.
        let mut block = Block::new(&self.slab, self.offset(addr), addr, len);
        block.as_mut().fill(0);
        Ok(block)
    }

    /// Return a block. Panics if it does not belong to this buffer.
    ///
    /// A block at the head or in the second-chance region becomes a tombstone;
    /// any other block becomes a hole for its class. The head then moves past
    /// every dead block at the front. A claimed block stays claimed until its
    /// evictor retires it.
    pub fn free(&self, block: Block) {
        assert!(
            self.slab.contains(block.ptr(), block.len()),
            "block does not belong to this circular buffer"
        );
        let addr = block.addr();
        let expected_ptr = self
            .slab
            .range(self.offset(addr) + BLOCK_HEADER_BYTES, block.len());
        assert_eq!(
            block.ptr(),
            expected_ptr,
            "block pointer does not match its logical address"
        );

        let class = self.class(block.len());
        self.lock_state()
            .free_block(&self.slab, addr, block.span(), class, self.second_chance);
    }

    /// Claim the oldest live block for eviction. Dead blocks at the head are
    /// passed first. The claimed block is never reused, and the head waits
    /// until it is retired or unclaimed.
    pub fn claim_victim(&self) -> Claim {
        let mut state = self.lock_state();
        state.advance_head(&self.slab);
        state.claim_oldest(&self.slab)
    }

    /// Give a claim back without evicting. If the owner freed the block in
    /// the meantime, this retires it instead, so it cannot become a live
    /// block without a handle.
    pub fn unclaim(&self, victim: Victim) {
        self.lock_state().unclaim(&self.slab, &victim);
    }

    /// Finish an eviction. The owner must have returned the block with
    /// `free` while it was claimed; this panics otherwise, because the head
    /// would pass bytes that a handle still covers.
    pub fn retire(&self, victim: Victim) {
        self.lock_state().retire(&self.slab, &victim);
    }

    /// Whether the block at logical address `addr` is in the second-chance
    /// region. Reads the tail without the mutex, so the answer may be a few
    /// allocations stale, which only shifts which block gets a second chance.
    pub fn in_second_chance(&self, addr: usize) -> bool {
        self.tail.load(Ordering::Relaxed).saturating_sub(addr) >= self.second_chance
    }

    /// Whether a block of `size` bytes fits at the tail now, including a
    /// possible filler. Holes are not counted.
    pub fn has_room(&self, size: usize) -> bool {
        self.lock_state()
            .fits(self.capacity, BLOCK_HEADER_BYTES + size)
    }

    /// Bytes between head and tail, dead blocks included.
    pub fn used(&self) -> usize {
        self.lock_state().used()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn metrics(&self) -> Metrics {
        self.lock_state().metrics
    }

    /// The index of the class `size`. Panics if `size` is not a class: a
    /// reused hole has exactly its class's size, so any other size could
    /// overlap the next block.
    fn class(&self, size: usize) -> usize {
        self.classes
            .binary_search(&size)
            .unwrap_or_else(|_| panic!("size {size} is not a class"))
    }

    /// The slab offset of a logical address.
    fn offset(&self, addr: usize) -> usize {
        addr & (self.capacity - 1)
    }

    fn lock_state(&self) -> MutexGuard<'_, State> {
        self.state.lock()
    }
}

impl Drop for CircularBuffer {
    fn drop(&mut self) {
        let state = self.state.get_mut();
        if state.outstanding != 0 {
            // Blocks still point into the slab; keep their bytes valid.
            self.slab.leak();
        }
    }
}

#[cfg(test)]
mod tests;
