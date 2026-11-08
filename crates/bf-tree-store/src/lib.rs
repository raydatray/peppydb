//! Durable homes for leaf pages, addressed by [`PageNumber`].
//!
//! A [`PageStore`] owns fixed-size pages, a [`Superblock`] that records the
//! page size and the tree configuration the pages were written under, and
//! the committed [`State`]. In a file, two 4096-byte superblock copies come
//! first and page number `i` lives at byte offset
//! `PAGES_OFFSET + i * page_size`, so a store is self-describing and needs
//! no catalogue.
//!
//! A page number is a position, not a leaf: the tree stamps each stored page
//! with the leaf it belongs to. A new page is all zeros; a freed page keeps
//! its bytes until it is reused.
//!
//! ```
//! use bf_tree_store::{MemStore, PageNumber, PageStore, Superblock};
//!
//! let superblock = Superblock {
//!     page_size: 256,
//!     inner_capacity: 256,
//!     max_key_len: 8,
//!     max_value_len: 16,
//! };
//! let store = MemStore::new(superblock).unwrap();
//! let page_number = store.alloc().unwrap();
//! assert_eq!(page_number, PageNumber::new(0).unwrap());
//!
//! let page = vec![7; 256];
//! store.write(page_number, &page).unwrap();
//! let mut back = vec![0; 256];
//! store.read(page_number, &mut back).unwrap();
//! assert_eq!(back, page);
//! assert_eq!(store.page_count(), 1);
//! ```

mod file;
mod mem;
mod superblock;

pub use file::{FileStore, Io};
pub use mem::MemStore;
#[cfg(test)]
use superblock::ENCODED_BYTES;

/// The position of a page in the store. Page numbers are not leaves: a
/// leaf's page may move to another page number, and the page header names
/// the leaf.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageNumber(u64);

impl PageNumber {
    /// `None` for `u64::MAX`, which callers use as "no page".
    pub const fn new(raw: u64) -> Option<Self> {
        if raw == u64::MAX {
            None
        } else {
            Some(Self(raw))
        }
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    fn index(self) -> Option<usize> {
        usize::try_from(self.0).ok()
    }
}

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
    #[error("page {} was never allocated", .0.raw())]
    UnknownPage(PageNumber),
    #[error("page {} is free", .0.raw())]
    FreedPage(PageNumber),
    #[error("buffer length {actual} does not match the page size {expected}")]
    BufferLength { expected: usize, actual: usize },
    #[error("store file length is not a whole number of pages")]
    TruncatedFile,
    #[error("neither superblock copy is intact")]
    NoSuperblock,
    /// Direct I/O was requested where it is unavailable.
    #[error("direct I/O unavailable: {0}")]
    DirectIo(&'static str),
    #[error("commit of checkpoint {got} does not follow checkpoint {current}")]
    CommitOrder { current: u64, got: u64 },
    #[error("I/O error during {op}: {kind}{}", .code.map(|code| format!(" (os error {code})")).unwrap_or_default())]
    Io {
        op: &'static str,
        kind: std::io::ErrorKind,
        code: Option<i32>,
    },
}

/// Fixed-size pages addressed by page number.
///
/// Reads and writes transfer exactly one page; the buffer length must equal
/// [`PageStore::page_size`]. Writes are durable only after [`PageStore::sync`]
/// returns.
///
/// A store is shared between threads. It does not order concurrent accesses
/// to one page; the caller must serialize them (the tree does so with the
/// leaf lock).
pub trait PageStore: Send + Sync + std::fmt::Debug {
    /// Length of every page in bytes; equal to the superblock's `page_size`.
    fn page_size(&self) -> usize;

    /// Number of page numbers ever allocated, live or freed:
    /// `0..page_count()`.
    fn page_count(&self) -> u64;

    /// Hand out a page: a freed one if any (its old bytes kept), else a new
    /// zero page at the end.
    fn alloc(&self) -> Result<PageNumber, StoreError>;

    /// Return a page for reuse. Its bytes are not changed.
    fn free(&self, page_number: PageNumber) -> Result<(), StoreError>;

    /// Copy a page into `buf`. A freed page reads as its old bytes.
    fn read(&self, page_number: PageNumber, buf: &mut [u8]) -> Result<(), StoreError>;

    /// Replace a live page's bytes with `buf`.
    fn write(&self, page_number: PageNumber, buf: &[u8]) -> Result<(), StoreError>;

    /// Read `buf.len() / page_size()` consecutive pages starting at
    /// `first`, live or freed. `buf.len()` must be a multiple of the page
    /// size. The default reads one page at a time; a file store reads the
    /// run with one positioned read.
    fn read_run(&self, first: PageNumber, buf: &mut [u8]) -> Result<(), StoreError> {
        let page_size = self.page_size();
        if !buf.len().is_multiple_of(page_size) {
            return Err(StoreError::BufferLength {
                expected: buf.len().next_multiple_of(page_size),
                actual: buf.len(),
            });
        }
        buf.chunks_mut(page_size)
            .zip(first.raw()..)
            .try_for_each(|(page, raw)| {
                let page_number = PageNumber::new(raw).ok_or(StoreError::UnknownPage(first))?;
                self.read(page_number, page)
            })
    }

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
