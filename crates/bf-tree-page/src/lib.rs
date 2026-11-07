//! Packed pages backed by caller-owned storage.
//!
//! ```
//! use bf_tree_page::{Page, RecordState};
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

use std::ops::Range;

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use bf_tree_node_id::LeafId;

mod compact;
mod fence;
mod key;
mod layout;
mod merge;
mod read;
mod record;
mod split;
mod validate;
mod write;

pub use fence::{Fence, Fences};
pub use key::KeyRef;
use layout::{
    COUNT_MASK, CRC_RANGE, HAS_FENCES, HEADER_BYTES, HIGH_INFINITY, Header, LOW_INFINITY,
    NO_BACKING_LEAF, NO_LEAF, Slot,
};
pub use read::Iter;
pub use record::{RecordKind, RecordRef, RecordState};
pub use split::{LeafSplit, Merge};
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
    #[error("split destination capacity must equal the source capacity")]
    CapacityMismatch,
    #[error("records cannot form two fenced pages that fit and each keep a record")]
    CannotSplit,
    /// A merge batch is unsorted or holds a clean record.
    #[error("invalid merge batch: {0}")]
    InvalidBatch(&'static str),
    /// Stored bytes failed [`Page::validate`]; the reason names the first
    /// check that failed.
    #[error("corrupt page: {0}")]
    Corrupt(&'static str),
    /// A leaf ID does not fit the page header's 32-bit `leaf` field.
    #[error("leaf ID does not fit the page header's 32-bit leaf field")]
    LeafIdTooLarge,
}

/// A leaf-level page viewed in place over caller-owned bytes: the header
/// followed by the slot directory, free space, and payloads. It is the base type
/// for base leaf pages, full-page caches, and mini-pages. Any buffer alignment
/// works.
///
/// Methods assume the bytes came from [`Page::init`] or [`Page::init_with_fences`],
/// or passed [`Page::validate`]. Viewing other bytes as a `Page` through the
/// public `zerocopy` traits can make them panic.
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
        page.header.leaf.set(NO_LEAF);

        Ok(page)
    }

    /// The leaf a mini-page overlays, or `None` for a base page.
    pub fn backing_leaf(&self) -> Option<LeafId> {
        LeafId::new(self.header.backing_leaf.get())
    }

    /// Record the leaf this page overlays, or clear it.
    pub fn set_backing_leaf(&mut self, leaf: Option<LeafId>) {
        self.header
            .backing_leaf
            .set(leaf.map_or(NO_BACKING_LEAF, LeafId::raw));
    }

    /// The leaf this stored base page belongs to, or `None`. Only pages the
    /// tree has written to its store carry one.
    pub fn leaf(&self) -> Option<LeafId> {
        match self.header.leaf.get() {
            NO_LEAF => None,
            raw => LeafId::new(u64::from(raw)),
        }
    }

    /// Stamp the leaf this page belongs to, or clear it. The field holds 32
    /// bits, so a raw ID of `u32::MAX` or more is rejected.
    pub fn set_leaf(&mut self, leaf: Option<LeafId>) -> Result<(), PageError> {
        let raw = match leaf {
            None => NO_LEAF,
            Some(leaf) => u32::try_from(leaf.raw())
                .ok()
                .filter(|&raw| raw != NO_LEAF)
                .ok_or(PageError::LeafIdTooLarge)?,
        };
        self.header.leaf.set(raw);
        Ok(())
    }

    /// CRC32C of every byte of the page, with the `crc` field taken as zero.
    pub fn checksum(&self) -> u32 {
        checksum(self.as_bytes())
    }

    /// The `crc` field as stored.
    pub fn stored_checksum(&self) -> u32 {
        self.header.crc.get()
    }

    /// Store [`Page::checksum`] in the `crc` field. Call after every other
    /// change to the page.
    pub fn seal(&mut self) {
        let crc = self.checksum();
        self.header.crc.set(crc);
    }

    /// Whether the stored checksum matches the bytes.
    pub fn verify_checksum(&self) -> bool {
        self.stored_checksum() == self.checksum()
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

    fn prefix_len(&self) -> usize {
        usize::from(self.header.prefix_len.get())
    }

    /// Reads the low fence's payload directly, because `fences()` needs the
    /// prefix to decode the high fence. A nonzero prefix implies a finite low
    /// fence, so this never treats a sentinel as an offset.
    fn prefix(&self) -> &[u8] {
        let len = self.prefix_len();
        if len == 0 {
            return &[];
        }

        let start = usize::from(self.slots()[0].offset.get());
        &self.as_bytes()[start..start + len]
    }

    /// Stored suffix of an ordinary record or finite high fence.
    fn suffix_range(&self, slot: &Slot) -> Range<usize> {
        slot.stored_key_range(self.prefix_len())
    }

    /// Stored key and value bytes of slot `index`, counting fence slots, or
    /// `None` for an infinite fence.
    fn payload_range(&self, index: usize) -> Option<Range<usize>> {
        let slot = self.slots()[index];
        let omitted = match (self.record_start(), index) {
            (2, 0) if slot.offset.get() == LOW_INFINITY => return None,
            (2, 1) if slot.offset.get() == HIGH_INFINITY => return None,
            (2, 0) => 0,
            _ => self.prefix_len(),
        };

        let key = slot.stored_key_range(omitted);
        Some(key.start..key.end + slot.value_len())
    }

    pub fn check_capacity(capacity: usize) -> Result<(), PageError> {
        if !(64..=4096).contains(&capacity) || !capacity.is_multiple_of(64) {
            return Err(PageError::InvalidCapacity);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests;

/// Whether `bytes`, a stored page that may be damaged, carries a matching
/// checksum. Reads only the `crc` field and the bytes, never the capacity, so
/// it is safe on any buffer of at least `HEADER_BYTES` bytes.
pub fn verify_stored_checksum(bytes: &[u8]) -> bool {
    let Some(stored) = bytes.get(CRC_RANGE) else {
        return false;
    };
    u32::from_le_bytes(stored.try_into().expect("four bytes")) == checksum(bytes)
}

/// CRC32C of `bytes` with the `crc` field taken as zero.
fn checksum(bytes: &[u8]) -> u32 {
    let crc = crc32c::crc32c(&bytes[..CRC_RANGE.start]);
    let crc = crc32c::crc32c_append(crc, &[0; 4]);
    crc32c::crc32c_append(crc, &bytes[CRC_RANGE.end..])
}
