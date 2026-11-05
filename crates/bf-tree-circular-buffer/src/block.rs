//! `Block`: the handle to one byte range of the slab.
//!
//! With `slab.rs`, the only module with `unsafe` code. Here it builds slices
//! from a block's pointer and declares `Block` `Send` and `Sync`.

#![allow(unsafe_code)]

use std::ptr::NonNull;

use super::BLOCK_HEADER_BYTES;
use super::slab::Slab;

/// A live block: the only handle to its byte range in the circular buffer.
///
/// The bytes are reached through `AsRef<[u8]>` and `AsMut<[u8]>`, so the
/// borrow checker enforces exclusive access through this handle. Return the
/// block with [`CircularBuffer::free`](super::CircularBuffer::free); dropping
/// it instead leaks its space until the buffer is dropped.
#[must_use = "return the block with CircularBuffer::free"]
#[derive(Debug)]
pub struct Block {
    /// The block's first byte: the 16-byte header, then the payload.
    ptr: NonNull<u8>,
    /// Logical address; increases forever across laps of the slab.
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
    /// A handle to the header and payload at `offset` in `slab`. Panics if
    /// the range does not lie in the slab.
    ///
    /// The caller, the circular buffer, guarantees that no other live handle
    /// covers the range and that the slab outlives the handle; slices built
    /// from the block rely on both.
    pub(super) fn new(slab: &Slab, offset: usize, addr: usize, seq: u64, len: u32) -> Self {
        let ptr = slab.range(offset, BLOCK_HEADER_BYTES + len as usize);
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

    fn whole(&self) -> &[u8] {
        // SAFETY: `[ptr, ptr + span)` lies in the slab (checked in `new`), the
        // slab outlives every block (it is leaked if blocks remain at drop),
        // its bytes are initialized (allocated zeroed, written only as bytes),
        // and no other handle covers the range. `&self` excludes a mutable
        // borrow through this handle. `span` is at most the slab size, which
        // `Layout` bounds by `isize::MAX`.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.span()) }
    }

    fn whole_mut(&mut self) -> &mut [u8] {
        // SAFETY: as in `whole`; `&mut self` makes this the only borrow of the
        // range.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.span()) }
    }

    /// The 16 header bytes before the payload. They hold a debug canary.
    pub(super) fn header(&self) -> &[u8] {
        &self.whole()[..BLOCK_HEADER_BYTES]
    }

    pub(super) fn header_mut(&mut self) -> &mut [u8] {
        &mut self.whole_mut()[..BLOCK_HEADER_BYTES]
    }
}

impl AsRef<[u8]> for Block {
    fn as_ref(&self) -> &[u8] {
        &self.whole()[BLOCK_HEADER_BYTES..]
    }
}

impl AsMut<[u8]> for Block {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.whole_mut()[BLOCK_HEADER_BYTES..]
    }
}
