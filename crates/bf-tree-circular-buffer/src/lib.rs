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
//! Poisoning is ignored: a panic inside a critical section has already failed
//! its own operation and must not fail every later caller. To keep that safe,
//! every critical section follows one rule, **check before mutating**: all
//! checks and assertions run before the first write to the metadata, so a
//! panic leaves it consistent.

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use bf_tree_node_id::LeafId;

mod block;
mod slab;
mod state;

pub use block::Block;
use slab::Slab;
use state::{BlockState, State};

/// Bookkeeping bytes charged to every block on top of its size.
pub const BLOCK_HEADER_BYTES: usize = 16;

/// What a block holds; recorded for eviction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Mini,
    Full,
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
    /// A circular buffer of `capacity` bytes serving blocks of the sizes in
    /// `classes`.
    ///
    /// `capacity` must be a power of two of at least twice the largest class
    /// plus its 16-byte charge, so that an empty buffer always fits the
    /// largest block even after a filler. `classes` must be nonempty, nonzero,
    /// and strictly ascending. `second_chance_percent` is the share of the
    /// buffer, at the head, from which freed blocks are not reused; it must be
    /// at most 100.
    pub fn new(
        capacity: usize,
        classes: &[usize],
        second_chance_percent: u32,
    ) -> Result<Self, CircularBufferError> {
        let (Some(&smallest), Some(&largest)) = (classes.first(), classes.last()) else {
            return Err(CircularBufferError::InvalidClasses("no classes"));
        };
        if smallest == 0 {
            return Err(CircularBufferError::InvalidClasses("a class is zero"));
        }
        if classes.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(CircularBufferError::InvalidClasses(
                "classes are not strictly ascending",
            ));
        }
        if u32::try_from(largest).is_err() {
            return Err(CircularBufferError::InvalidClasses("a class exceeds u32"));
        }
        if !capacity.is_power_of_two() {
            return Err(CircularBufferError::InvalidCapacity("not a power of two"));
        }
        let minimum = largest
            .checked_add(BLOCK_HEADER_BYTES)
            .and_then(|span| span.checked_mul(2));
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
        let _ = (kind, owner);
        let len = u32::try_from(size).expect("classes fit in u32");

        let (seq, addr) = {
            let mut state = self.lock_state();
            let reserved = match state.take_hole(class, self.second_chance) {
                Some(hole) => {
                    state.metrics.reuses += 1;
                    hole
                }
                None => {
                    let bumped = state
                        .bump(self.capacity, BLOCK_HEADER_BYTES + size)
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
        let mut block = Block::new(&self.slab, self.offset(addr), addr, seq, len);
        block.as_mut().fill(0);
        write_canary(&mut block);
        Ok(block)
    }

    /// Return a block. Panics if it does not belong to this buffer.
    ///
    /// A block at the head or in the second-chance region becomes a tombstone;
    /// any other block becomes a hole for its class. The head then moves past
    /// every dead block at the front.
    pub fn free(&self, block: Block) {
        assert!(
            self.slab.contains(block.ptr(), block.span()),
            "block does not belong to this circular buffer"
        );
        check_canary(&block);

        let mut state = self.lock_state();
        let meta = state
            .get(block.seq())
            .filter(|meta| meta.addr == block.addr() && meta.span == block.span());
        assert!(
            meta.is_some_and(|meta| meta.state == BlockState::Live),
            "block is not a live block of this circular buffer"
        );

        let class = self.class(block.as_ref().len());
        let seq = block.seq();
        if state.is_head(seq) || state.in_region(block.addr(), self.second_chance) {
            state.get_mut(seq).expect("checked above").state = BlockState::Tombstone;
            state.metrics.tombstones += 1;
        } else {
            state.add_hole(seq, class);
        }
        state.outstanding -= 1;
        state.metrics.frees += 1;
        state.advance_head();
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
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for CircularBuffer {
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        if state.outstanding != 0 {
            // Blocks still point into the slab; keep their bytes valid.
            self.slab.leak();
        }
    }
}

/// Debug builds record each block's sequence number and length in its
/// header, and `free` checks them, to catch errors in the address arithmetic.
const CANARY: [u8; 4] = *b"CBUF";

fn canary(block: &Block) -> [u8; BLOCK_HEADER_BYTES] {
    let mut header = [0; BLOCK_HEADER_BYTES];
    header[..4].copy_from_slice(&CANARY);
    header[4..8].copy_from_slice(&(block.as_ref().len() as u32).to_le_bytes());
    header[8..].copy_from_slice(&block.seq().to_le_bytes());
    header
}

fn write_canary(block: &mut Block) {
    if cfg!(debug_assertions) {
        let header = canary(block);
        block.header_mut().copy_from_slice(&header);
    }
}

fn check_canary(block: &Block) {
    if cfg!(debug_assertions) {
        assert_eq!(
            block.header(),
            canary(block),
            "block header was overwritten"
        );
    }
}

#[cfg(test)]
mod tests;
