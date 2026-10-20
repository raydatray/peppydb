//! Packed pages backed by caller-owned storage.
//!
//! ```
//! use peppydb_bftree::{Page, RecordState};
//!
//! let mut storage = [0; 128];
//! let page = Page::init(&mut storage).unwrap();
//! page.try_insert(b"30", b"cat").unwrap();
//! page.try_insert(b"10", b"dog").unwrap();
//! page.try_insert(b"20", b"owl").unwrap();
//!
//! // Reads need only a shared reference.
//! let page: &Page = page;
//! assert_eq!(page.get(b"20").unwrap().unwrap().state, RecordState::Insert(b"owl"));
//! let records: Vec<_> = page.iter().map(|record| (record.key, record.state.value())).collect();
//! assert_eq!(records, vec![
//!     (b"10".as_slice(), Some(b"dog".as_slice())),
//!     (b"20".as_slice(), Some(b"owl".as_slice())),
//!     (b"30".as_slice(), Some(b"cat".as_slice())),
//! ]);
//! for record in page {
//!     assert_eq!(page.get(record.key), Ok(Some(record)));
//! }
//! ```

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

mod layout;
mod read;
mod record;
mod write;

use layout::{COUNT_MASK, HEADER_BYTES, Header, NO_BACKING_LEAF, Slot};
pub use read::Iter;
pub use record::{RecordKind, RecordRef, RecordState};

/// An operation rejected before changing any page bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PageError {
    #[error("page capacity must be a multiple of 64 from 64 through 4096 bytes")]
    InvalidCapacity,
    #[error("key must not be empty")]
    EmptyKey,
    #[error("record does not fit in an empty page")]
    RecordTooLarge,
    #[error("key already exists in the page")]
    DuplicateKey,
    /// The record fits an empty page, but not the remaining free space.
    #[error("page has insufficient free space for the record")]
    PageFull,
}

/// A leaf-level page viewed in place over caller-owned bytes: the header
/// followed by the slot directory, free space, and payloads. It is the base type
/// for base leaf pages, full-page caches, and mini-pages. Any buffer alignment
/// works.
///
/// Methods assume the bytes came from [`Page::init`]; viewing other bytes as a
/// `Page` through the public `zerocopy` traits can make them panic.
#[derive(Debug, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct Page {
    header: Header,
    /// The slot directory, free space, and payloads.
    data: [u8],
}

impl Page {
    /// Zero the buffer and initialize an empty page that borrows it.
    ///
    /// Invalid capacities leave the supplied buffer unchanged.
    pub fn init(bytes: &mut [u8]) -> Result<&mut Self, PageError> {
        let capacity = bytes.len();
        if !(64..=4096).contains(&capacity) || !capacity.is_multiple_of(64) {
            return Err(PageError::InvalidCapacity);
        }

        bytes.fill(0);

        let page = Self::mut_from_bytes(bytes).expect("capacity holds the header");
        // The validated capacity bounds both encoded sizes to u16.
        page.header.capacity.set(capacity as u16);
        page.header
            .remaining_size
            .set((capacity - HEADER_BYTES) as u16);
        page.header.backing_leaf.set(NO_BACKING_LEAF);

        Ok(page)
    }

    /// Number of records.
    pub fn len(&self) -> usize {
        usize::from(self.header.count_and_flags.get() & COUNT_MASK)
    }

    /// Whether the page contains no records.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total buffer length in bytes, including the header.
    pub fn capacity(&self) -> usize {
        usize::from(self.header.capacity.get())
    }

    /// Contiguous free space between the sorted slots and packed payloads.
    pub fn free_bytes(&self) -> usize {
        usize::from(self.header.remaining_size.get())
    }

    /// The sorted slot directory at the start of the record area.
    fn slots(&self) -> &[Slot] {
        let (slots, _): (&[Slot], _) =
            FromBytes::ref_from_prefix_with_elems(&self.data, self.len())
                .expect("slot directory fits the page");

        slots
    }

    fn slots_mut(&mut self) -> &mut [Slot] {
        let count = self.len();
        let (slots, _): (&mut [Slot], _) =
            FromBytes::mut_from_prefix_with_elems(&mut self.data, count)
                .expect("slot directory fits the page");

        slots
    }
}

#[cfg(test)]
mod tests;
