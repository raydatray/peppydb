//! `Block`: the handle to one byte range of the slab.
//!
//! With `slab.rs`, the only module with `unsafe` code. Here it builds slices
//! from a block's pointer and declares `Block` `Send` and `Sync`.

#![allow(unsafe_code)]

use std::ptr::NonNull;

use super::slab::Slab;
use super::{BLOCK_HEADER_BYTES, Victim};

/// A live block: the only handle to its byte range in the circular buffer.
///
/// The bytes are reached through `AsRef<[u8]>` and `AsMut<[u8]>`, covering
/// only the payload (never the leading header bytes). The borrow checker
/// enforces exclusive access through this handle. Return the block with
/// [`CircularBuffer::free`](super::CircularBuffer::free); dropping it instead
/// leaks its space until the buffer is dropped.
#[must_use = "return the block with CircularBuffer::free"]
#[derive(Debug)]
pub struct Block {
    /// The payload's first byte (immediately after the block header).
    ptr: NonNull<u8>,
    /// Logical address of the block's header; increases forever across laps of
    /// the slab.
    addr: usize,
    seq: u64,
    /// Payload length: exactly the requested size.
    len: u32,
}

// SAFETY: a `Block` is the only handle to its byte range (the circular
// buffer never hands out overlapping live ranges), so moving it to another
// thread moves exclusive access to those bytes.
unsafe impl Send for Block {}

// SAFETY: `&Block` gives only `&[u8]`; shared reads of a range that no other
// handle covers are data-race free.
unsafe impl Sync for Block {}

impl Block {
    /// A handle to the payload of a block whose header starts at `offset` in
    /// `slab`. Panics if the header and payload do not lie in the slab.
    ///
    /// The caller, the circular buffer, guarantees that no other live handle
    /// covers the payload range and that the slab outlives the handle; slices
    /// built from the block rely on both.
    pub(super) fn new(slab: &Slab, offset: usize, addr: usize, seq: u64, len: u32) -> Self {
        let payload_offset = offset
            .checked_add(BLOCK_HEADER_BYTES)
            .expect("payload offset overflows");
        let ptr = slab.range(payload_offset, len as usize);
        Self {
            ptr,
            addr,
            seq,
            len,
        }
    }

    /// The block's logical address in the circular buffer.
    pub fn addr(&self) -> usize {
        self.addr
    }

    /// Whether this is the block that `victim` names.
    pub fn is(&self, victim: &Victim) -> bool {
        self.seq == victim.seq()
    }

    pub(super) fn seq(&self) -> u64 {
        self.seq
    }

    pub(super) fn ptr(&self) -> NonNull<u8> {
        self.ptr
    }

    /// Header plus payload length.
    pub(super) fn span(&self) -> usize {
        BLOCK_HEADER_BYTES + self.len as usize
    }

    /// Payload length.
    pub(super) fn len(&self) -> usize {
        self.len as usize
    }
}

impl AsRef<[u8]> for Block {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: `[ptr, ptr + len)` lies in the slab (checked in `new`), the
        // slab outlives every block (it is leaked if blocks remain at drop),
        // its bytes are initialized (allocated zeroed, written only as bytes),
        // and no other handle covers the payload range. `&self` excludes a
        // mutable borrow through this handle. `len` is bounded by the slab
        // size, which `Layout` bounds by `isize::MAX`.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len as usize) }
    }
}

impl AsMut<[u8]> for Block {
    fn as_mut(&mut self) -> &mut [u8] {
        // SAFETY: as in `AsRef::as_ref`; `&mut self` makes this the only
        // borrow of the payload range.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len as usize) }
    }
}
