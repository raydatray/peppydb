//! A bounded circular buffer of variable-size blocks (BF-Tree paper, §4).
//!
//! Every block lives in one fixed allocation, the slab. Blocks are allocated
//! at the tail; the head moves forward over dead blocks, and the bytes between
//! head and tail are the budget in use. A block never crosses the end of the
//! slab: when it would, a dead filler block covers the rest of the slab first.
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
    capacity: usize,
    /// Allocation sizes, strictly ascending.
    classes: Box<[usize]>,
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

        Ok(Self {
            slab: Slab::new(capacity),
            state: Mutex::new(State::default()),
            capacity,
            classes: classes.into(),
        })
    }

    /// Allocate a zeroed block of exactly `size` bytes, which must be one of
    /// the classes, for `owner`.
    pub fn alloc(
        &self,
        size: usize,
        kind: Kind,
        owner: LeafId,
    ) -> Result<Block, CircularBufferFull> {
        debug_assert!(self.classes.contains(&size), "size {size} is not a class");
        let _ = (kind, owner);
        let len = u32::try_from(size).expect("classes fit in u32");

        let (seq, addr) = {
            let mut state = self.lock_state();
            let reserved = state
                .bump(self.capacity, BLOCK_HEADER_BYTES + size)
                .ok_or(CircularBufferFull)?;
            state.outstanding += 1;
            state.metrics.allocs += 1;
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
    /// The block becomes a tombstone, and the head moves past every dead
    /// block at the front.
    pub fn free(&self, block: Block) {
        assert!(
            self.slab.contains(block.ptr(), block.span()),
            "block does not belong to this circular buffer"
        );
        check_canary(&block);

        let mut state = self.lock_state();
        let meta = state
            .get(block.seq())
            .filter(|meta| meta.addr == block.addr());
        assert!(
            meta.is_some_and(|meta| meta.state == BlockState::Live),
            "block is not a live block of this circular buffer"
        );

        let meta = state.get_mut(block.seq()).expect("checked above");
        meta.state = BlockState::Tombstone;
        state.outstanding -= 1;
        state.metrics.frees += 1;
        state.metrics.tombstones += 1;
        state.advance_head();
    }

    /// Whether a block of `size` bytes fits at the tail now, including a
    /// possible filler.
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
