//! Durable homes for leaf pages, addressed by [`LeafId`].
//!
//! A [`PageStore`] owns fixed-size page slots, a [`Superblock`] that records
//! the page size and the tree configuration the pages were written under,
//! and the committed [`State`]. In a file, two 4096-byte superblock copies
//! come first and slot `id` lives at byte offset
//! `PAGES_OFFSET + id * page_size`, so a store is self-describing and needs
//! no catalogue.
//! A freed slot is all zeros; a valid page is never all zeros because its
//! header carries a nonzero capacity, so free slots are recoverable by
//! whoever reads every slot, such as `Tree::open`.
//!
//! ```
//! use bf_tree_node_id::LeafId;
//! use bf_tree_store::{MemStore, PageStore, Superblock};
//!
//! let superblock = Superblock {
//!     page_size: 256,
//!     inner_capacity: 256,
//!     max_key_len: 8,
//!     max_value_len: 16,
//! };
//! let store = MemStore::new(superblock).unwrap();
//! let id = store.alloc().unwrap();
//! assert_eq!(id, LeafId::new(0).unwrap());
//!
//! let page = vec![7; 256];
//! store.write(id, &page).unwrap();
//! let mut back = vec![0; 256];
//! store.read(id, &mut back).unwrap();
//! assert_eq!(back, page);
//! assert_eq!(store.slots(), 1);
//! ```

use bf_tree_node_id::LeafId;

mod file;
mod mem;
mod superblock;

pub use file::FileStore;
pub use mem::MemStore;
#[cfg(test)]
use superblock::ENCODED_BYTES;
pub use superblock::{COPY_BYTES, MAGIC, PAGES_OFFSET, State, Superblock, VERSION};

/// A rejected store operation.
///
/// The variants are `Copy` so that tree errors stay `Copy`; an I/O failure
/// keeps its [`std::io::ErrorKind`] and raw OS code rather than the boxed
/// error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("invalid superblock: {0}")]
    InvalidSuperblock(&'static str),
    #[error("slot {} was never allocated", .0.raw())]
    UnknownSlot(LeafId),
    #[error("slot {} is free", .0.raw())]
    FreedSlot(LeafId),
    #[error("buffer length {actual} does not match the page size {expected}")]
    BufferLength { expected: usize, actual: usize },
    #[error("store file length is not a whole number of pages")]
    TruncatedFile,
    #[error("neither superblock copy is intact")]
    NoSuperblock,
    #[error("commit of checkpoint {got} does not follow checkpoint {current}")]
    CommitOrder { current: u64, got: u64 },
    #[error("I/O error during {op}: {kind}{}", .code.map(|code| format!(" (os error {code})")).unwrap_or_default())]
    Io {
        op: &'static str,
        kind: std::io::ErrorKind,
        code: Option<i32>,
    },
}

/// Fixed-size page slots behind stable leaf IDs.
///
/// Reads and writes transfer exactly one page; the buffer length must equal
/// [`PageStore::page_size`]. Writes are durable only after [`PageStore::sync`]
/// returns.
///
/// A store is shared between threads. It does not order concurrent accesses
/// to one slot; the caller must serialize them (the tree does so with the
/// leaf lock).
pub trait PageStore: Send + Sync + std::fmt::Debug {
    /// Length of every slot in bytes; equal to the superblock's `page_size`.
    fn page_size(&self) -> usize;

    /// Number of page slots ever allocated, including freed ones. Live and
    /// freed slot IDs are `0..slots()`.
    fn slots(&self) -> u64;

    /// Hand out a zeroed slot: a freed slot if any, else a new one at the end.
    fn alloc(&self) -> Result<LeafId, StoreError>;

    /// Zero a slot and return it for reuse.
    fn free(&self, id: LeafId) -> Result<(), StoreError>;

    /// Copy a slot into `buf`. A freed slot reads as zeros.
    fn read(&self, id: LeafId, buf: &mut [u8]) -> Result<(), StoreError>;

    /// Replace a live slot's bytes with `buf`.
    fn write(&self, id: LeafId, buf: &[u8]) -> Result<(), StoreError>;

    /// Make every completed write durable.
    fn sync(&self) -> Result<(), StoreError>;

    fn superblock(&self) -> &Superblock;

    /// The last committed state.
    fn state(&self) -> State;

    /// Durably and atomically replace the committed state. The caller syncs
    /// its page writes first. `state.checkpoint_id` must be one more than
    /// the current one; otherwise the result is `CommitOrder`, which catches
    /// two callers committing at once.
    fn commit(&self, state: &State) -> Result<(), StoreError>;
}

/// Check that `next` succeeds `current`.
fn check_commit_order(current: &State, next: &State) -> Result<(), StoreError> {
    if current.checkpoint_id.checked_add(1) != Some(next.checkpoint_id) {
        return Err(StoreError::CommitOrder {
            current: current.checkpoint_id,
            got: next.checkpoint_id,
        });
    }
    Ok(())
}

/// Check that `buf` is exactly one page long.
fn check_buffer(page_size: usize, buf: &[u8]) -> Result<(), StoreError> {
    if buf.len() != page_size {
        return Err(StoreError::BufferLength {
            expected: page_size,
            actual: buf.len(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests;
