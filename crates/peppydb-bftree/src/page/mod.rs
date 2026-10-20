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
//!
//! // Keys are borrowed logical views; materialize them only when needed.
//! let records: Vec<_> = page
//!     .iter()
//!     .map(|record| (record.key.to_vec(), record.state.value()))
//!     .collect();
//! assert_eq!(records, vec![
//!     (b"10".to_vec(), Some(b"dog".as_slice())),
//!     (b"20".to_vec(), Some(b"owl".as_slice())),
//!     (b"30".to_vec(), Some(b"cat".as_slice())),
//! ]);
//! assert!(page.iter().all(|record| record.key.cmp_bytes(b"05").is_gt()));
//! for record in page {
//!     assert_eq!(page.get(&record.key.to_vec()), Ok(Some(record)));
//! }
//! ```

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

mod compact;
mod fence;
mod key;
mod layout;
mod read;
mod record;
mod write;

pub use fence::{Fence, Fences};
pub use key::KeyRef;
use layout::{COUNT_MASK, HAS_FENCES, HEADER_BYTES, Header, NO_BACKING_LEAF, Slot};
pub use read::Iter;
pub use record::{RecordKind, RecordRef, RecordState};
pub use write::UpsertOutcome;

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
    /// The current physical path needs more contiguous space than is available.
    #[error("page has insufficient free space for the record")]
    PageFull,
    #[error("finite page fences must satisfy lower < upper")]
    InvalidFences,
    #[error("fence slots and keys do not fit in the page")]
    FencesTooLarge,
    #[error("key is outside the page fence range")]
    KeyOutOfRange,
    #[error("range start must not exceed its end")]
    InvalidRange,
}

/// A leaf-level page viewed in place over caller-owned bytes: the header
/// followed by the slot directory, free space, and payloads. It is the base type
/// for base leaf pages, full-page caches, and mini-pages. Any buffer alignment
/// works.
///
/// Methods assume the bytes came from [`Page::init`] or [`Page::init_with_fences`].
/// Viewing other bytes as a `Page` through the public `zerocopy` traits can make
/// them panic.
#[derive(Debug, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct Page {
    header: Header,
    data: [u8],
}

impl Page {
    /// Zero the buffer and initialize an empty page that borrows it.
    ///
    /// Invalid capacities leave the supplied buffer unchanged.
    pub fn init(bytes: &mut [u8]) -> Result<&mut Self, PageError> {
        let capacity = bytes.len();
        Self::check_capacity(capacity)?;

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

    /// Number of records, including tombstones and phantoms but not fences.
    pub fn len(&self) -> usize {
        self.slot_count() - self.record_start()
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

    fn slots(&self) -> &[Slot] {
        let (slots, _): (&[Slot], _) =
            FromBytes::ref_from_prefix_with_elems(&self.data, self.slot_count())
                .expect("slot directory fits the page");

        slots
    }

    fn slots_mut(&mut self) -> &mut [Slot] {
        let count = self.slot_count();
        let (slots, _): (&mut [Slot], _) =
            FromBytes::mut_from_prefix_with_elems(&mut self.data, count)
                .expect("slot directory fits the page");

        slots
    }

    fn slot_count(&self) -> usize {
        usize::from(self.header.count_and_flags.get() & COUNT_MASK)
    }

    fn record_start(&self) -> usize {
        if self.header.count_and_flags.get() & HAS_FENCES != 0 {
            return 2;
        }

        0
    }

    fn record_slots(&self) -> &[Slot] {
        &self.slots()[self.record_start()..]
    }

    fn check_capacity(capacity: usize) -> Result<(), PageError> {
        if !(64..=4096).contains(&capacity) || !capacity.is_multiple_of(64) {
            return Err(PageError::InvalidCapacity);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests;
