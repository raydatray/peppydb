//! Packed pages backed by caller-owned storage.

use zerocopy::little_endian::{U16, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

/// Encoded header length shared by every [`Page`].
const HEADER_BYTES: usize = 32;
const COUNT_MASK: u16 = 0x0fff;
const NO_BACKING_LEAF: u64 = u64::MAX;

/// Page header at bytes `0..HEADER_BYTES`.
#[derive(Debug, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
struct Header {
    /// Slot count in the low 12 bits; flags in the high 4 bits.
    count_and_flags: U16,
    /// Total page length in bytes.
    capacity: U16,
    /// Contiguous free bytes between the slot directory and the payloads.
    remaining_size: U16,
    /// Length of the prefix shared by all keys; zero until compression exists.
    prefix_len: U16,
    /// Location of the backing leaf, or `NO_BACKING_LEAF`.
    backing_leaf: U64,
    /// Log sequence number (LSN); inactive.
    lsn: U64,
    /// Snapshot version; inactive.
    snapshot_version: U64,
}

const _: () = assert!(size_of::<Header>() == HEADER_BYTES);

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
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests decode raw byte offsets rather than `Header` and `Slot`, so they
    // check the structs against the documented format independently.

    #[test]
    fn initialize_all_capacities_on_unaligned_buffers() {
        for capacity in (64..=4096).step_by(64) {
            // Offset by one byte so the header's u64 fields are misaligned.
            let mut storage = vec![0xa5; capacity + 1];
            let page = Page::init(&mut storage[1..]).unwrap();
            assert_eq!(page.len(), 0);
            assert!(page.is_empty());
            assert_eq!(page.capacity(), capacity);
            assert_eq!(page.free_bytes(), capacity - HEADER_BYTES);
            assert_eq!(&page.as_bytes()[0..2], &[0, 0]);
            assert_eq!(&page.as_bytes()[2..4], &(capacity as u16).to_le_bytes());
            assert_eq!(
                &page.as_bytes()[4..6],
                &((capacity - 32) as u16).to_le_bytes()
            );
            assert_eq!(&page.as_bytes()[6..8], &[0, 0]);
            assert_eq!(&page.as_bytes()[8..16], &[0xff; 8]);
            assert!(page.as_bytes()[16..].iter().all(|&byte| byte == 0));
            assert_eq!(storage[0], 0xa5);
        }
    }

    #[test]
    fn invalid_initialization_preserves_bytes() {
        for capacity in [0, 1, 16, 32, 63, 65, 96, 1000, 4095, 4097, 4160, 8192] {
            let mut storage = vec![0xa5; capacity];
            assert_eq!(
                Page::init(&mut storage).err(),
                Some(PageError::InvalidCapacity)
            );
            assert_eq!(storage, vec![0xa5; capacity]);
        }
    }

    #[test]
    fn errors_implement_std_error_with_messages() {
        let cases = [
            (
                PageError::InvalidCapacity,
                "page capacity must be a multiple of 64 from 64 through 4096 bytes",
            ),
            (PageError::EmptyKey, "key must not be empty"),
            (
                PageError::RecordTooLarge,
                "record does not fit in an empty page",
            ),
            (PageError::DuplicateKey, "key already exists in the page"),
            (
                PageError::PageFull,
                "page has insufficient free space for the record",
            ),
        ];
        for (error, message) in cases {
            let error: &dyn std::error::Error = &error;
            assert_eq!(error.to_string(), message);
        }
    }
}
