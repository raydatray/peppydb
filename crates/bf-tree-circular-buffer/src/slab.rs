//! The slab: one fixed allocation that every block lives in.
//!
//! This module and `block.rs` are the only places with `unsafe` code. Here it
//! allocates and frees the slab. Block pointers are derived with safe pointer
//! arithmetic after a bounds check.

#![allow(unsafe_code)]

use std::alloc::{self, Layout};
use std::ptr::NonNull;

/// Alignment of the slab. Pages need only alignment 1; 64 keeps each slab on
/// its own cache lines.
const ALIGN: usize = 64;

/// One zeroed allocation of `capacity` bytes.
#[derive(Debug)]
pub(super) struct Slab {
    base: NonNull<u8>,
    layout: Layout,
    /// Set when blocks are still outstanding at drop: the memory then stays
    /// allocated, so those blocks keep pointing at valid bytes.
    leaked: bool,
}

// SAFETY: the slab is plain bytes behind a pointer that only this crate
// dereferences. Byte access happens through `Block`s, which cover disjoint
// ranges and follow the borrow rules, so sharing or moving the base pointer
// between threads creates no data race.
unsafe impl Send for Slab {}

// SAFETY: see `Send`. `&Slab` only derives pointers and compares addresses.
unsafe impl Sync for Slab {}

impl Slab {
    /// Allocate `capacity` zeroed bytes. `capacity` must be nonzero and at
    /// most `isize::MAX`; the circular buffer validates both.
    pub(super) fn new(capacity: usize) -> Self {
        let layout = Layout::from_size_align(capacity, ALIGN).expect("validated capacity");
        assert!(layout.size() > 0, "slab capacity must be nonzero");

        // SAFETY: `layout` has a nonzero size, checked above.
        let ptr = unsafe { alloc::alloc_zeroed(layout) };
        let base = NonNull::new(ptr).unwrap_or_else(|| alloc::handle_alloc_error(layout));

        Self {
            base,
            layout,
            leaked: false,
        }
    }

    pub(super) fn capacity(&self) -> usize {
        self.layout.size()
    }

    /// A pointer to `offset` within the slab, for a range of `len` bytes that
    /// must lie inside it. Panics otherwise.
    pub(super) fn range(&self, offset: usize, len: usize) -> NonNull<u8> {
        let end = offset.checked_add(len).expect("range end overflows");
        assert!(end <= self.capacity(), "range exceeds the slab");

        // `wrapping_add` keeps the base pointer's provenance, and the offset
        // is in bounds, so the result points into the allocation.
        NonNull::new(self.base.as_ptr().wrapping_add(offset))
            .expect("in-bounds pointer is non-null")
    }

    /// Whether `[ptr, ptr + len)` lies inside the slab.
    pub(super) fn contains(&self, ptr: NonNull<u8>, len: usize) -> bool {
        let start = self.base.as_ptr().addr();
        let addr = ptr.as_ptr().addr();
        addr >= start
            && addr
                .checked_add(len)
                .is_some_and(|end| end <= start + self.capacity())
    }

    /// Keep the allocation alive after drop, because blocks still point into
    /// it.
    pub(super) fn leak(&mut self) {
        self.leaked = true;
    }
}

impl Drop for Slab {
    fn drop(&mut self) {
        if self.leaked {
            return;
        }
        // SAFETY: `base` came from `alloc_zeroed` with `layout` and is freed
        // only here, once. The owner calls `leak` unless no block is
        // outstanding, so no `Block` points into the memory after this.
        unsafe { alloc::dealloc(self.base.as_ptr(), self.layout) };
    }
}
