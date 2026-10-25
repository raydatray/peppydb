//! Durable homes for leaf pages, addressed by [`LeafId`].
//!
//! A [`PageStore`] owns fixed-size page slots and a [`Superblock`] that
//! records the page size and the tree configuration the pages were written
//! under. Slot `id` lives at byte offset `(id + 1) * page_size`, after a
//! one-page superblock, so a store is self-describing and needs no catalogue.
//! A freed slot is all zeros; a valid page is never all zeros because its
//! header carries a nonzero capacity, so free slots are recoverable by
//! whoever reads every slot, such as `Tree::open`.
//!
//! ```
//! use peppydb_bftree::{LeafId, MemStore, PageStore, Superblock};
//!
//! let superblock = Superblock {
//!     page_size: 256,
//!     inner_capacity: 256,
//!     max_key_len: 8,
//!     max_value_len: 16,
//! };
//! let mut store = MemStore::new(superblock).unwrap();
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

use zerocopy::little_endian::U32;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use crate::{LeafId, Page};

mod mem;

pub use mem::MemStore;

/// Magic bytes at the start of every store.
pub const MAGIC: [u8; 8] = *b"PEPPYBF\0";
/// The only superblock layout this crate reads and writes.
pub const VERSION: u32 = 1;

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
pub trait PageStore {
    /// Length of every slot in bytes; equal to the superblock's `page_size`.
    fn page_size(&self) -> usize;

    /// Number of page slots ever allocated, including freed ones. Live and
    /// freed slot IDs are `0..slots()`.
    fn slots(&self) -> u64;

    /// Hand out a zeroed slot: a freed slot if any, else a new one at the end.
    fn alloc(&mut self) -> Result<LeafId, StoreError>;

    /// Zero a slot and return it for reuse. Unused by this stack's tree; it
    /// exists so the boundary is stable when leaf merging arrives.
    fn free(&mut self, id: LeafId) -> Result<(), StoreError>;

    /// Copy a slot into `buf`. A freed slot reads as zeros.
    fn read(&self, id: LeafId, buf: &mut [u8]) -> Result<(), StoreError>;

    /// Replace a live slot's bytes with `buf`.
    fn write(&mut self, id: LeafId, buf: &[u8]) -> Result<(), StoreError>;

    /// Make every completed write durable.
    fn sync(&mut self) -> Result<(), StoreError>;

    fn superblock(&self) -> &Superblock;
}

/// The store's self-description: page size and the tree configuration that
/// every stored page was written under.
///
/// The configuration fixes the admission rule for the file. A tree opened
/// from the store must use it, so a reopened tree never admits a key the
/// stored pages cannot split.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Superblock {
    /// Slot length and leaf page capacity.
    pub page_size: usize,
    pub inner_capacity: usize,
    pub max_key_len: usize,
    pub max_value_len: usize,
}

/// On-disk superblock layout, version 1. Zero-padded to one page.
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
struct EncodedSuperblock {
    magic: [u8; 8],
    version: U32,
    page_size: U32,
    inner_capacity: U32,
    max_key_len: U32,
    max_value_len: U32,
}

const ENCODED_BYTES: usize = size_of::<EncodedSuperblock>();
const _: () = assert!(ENCODED_BYTES == 28);

impl Superblock {
    /// Check that the page size is a legal page capacity, which also makes it
    /// large enough to hold the encoded superblock.
    pub fn validate(&self) -> Result<(), StoreError> {
        Page::check_capacity(self.page_size)
            .map_err(|_| StoreError::InvalidSuperblock("page size is not a valid page capacity"))?;

        let fields = [self.inner_capacity, self.max_key_len, self.max_value_len];
        if fields
            .into_iter()
            .any(|field| u32::try_from(field).is_err())
        {
            return Err(StoreError::InvalidSuperblock(
                "configuration field exceeds u32",
            ));
        }

        Ok(())
    }

    /// Write the superblock into a zeroed `page_size`-byte buffer.
    pub fn encode(&self, page: &mut [u8]) -> Result<(), StoreError> {
        self.validate()?;
        if page.len() != self.page_size {
            return Err(StoreError::BufferLength {
                expected: self.page_size,
                actual: page.len(),
            });
        }

        page.fill(0);
        let encoded = EncodedSuperblock {
            magic: MAGIC,
            version: U32::new(VERSION),
            page_size: U32::new(self.page_size as u32),
            inner_capacity: U32::new(self.inner_capacity as u32),
            max_key_len: U32::new(self.max_key_len as u32),
            max_value_len: U32::new(self.max_value_len as u32),
        };
        page[..ENCODED_BYTES].copy_from_slice(encoded.as_bytes());

        Ok(())
    }

    /// Decode a superblock from the first page of a store. `page` may be
    /// longer than the encoded fields; trailing bytes are ignored.
    pub fn decode(page: &[u8]) -> Result<Self, StoreError> {
        let (encoded, _) = EncodedSuperblock::ref_from_prefix(page)
            .map_err(|_| StoreError::InvalidSuperblock("too short"))?;

        if encoded.magic != MAGIC {
            return Err(StoreError::InvalidSuperblock("bad magic"));
        }
        if encoded.version.get() != VERSION {
            return Err(StoreError::InvalidSuperblock("unsupported version"));
        }

        let superblock = Self {
            page_size: encoded.page_size.get() as usize,
            inner_capacity: encoded.inner_capacity.get() as usize,
            max_key_len: encoded.max_key_len.get() as usize,
            max_value_len: encoded.max_value_len.get() as usize,
        };
        superblock.validate()?;

        Ok(superblock)
    }
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
