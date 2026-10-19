//! Packed pages backed by caller-owned storage.

use std::ops::Range;

use zerocopy::little_endian::{U16, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

/// Encoded header length shared by every [`Page`].
const HEADER_BYTES: usize = 32;
const COUNT_MASK: u16 = 0x0fff;
const NO_BACKING_LEAF: u64 = u64::MAX;
const SLOT_BYTES: usize = 8;
const KEY_LEN_MASK: u16 = 0x3fff;
const VALUE_LEN_MASK: u16 = 0x7fff;

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

/// One entry in the sorted slot directory, which follows the header.
#[derive(Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
struct Slot {
    /// Page-relative offset of the key; the value follows the key.
    offset: U16,
    /// Record kind in the high 2 bits; key length in the low 14 bits.
    kind_and_key_len: U16,
    /// Reference bit in the high bit; value length in the low 15 bits.
    ref_and_value_len: U16,
    /// First two key bytes, zero-padded for one-byte keys.
    preview: [u8; 2],
}

const _: () = assert!(size_of::<Slot>() == SLOT_BYTES);

impl Slot {
    /// Page-relative byte range of the key, excluding state bits from its length.
    fn key_range(&self) -> Range<usize> {
        let start = usize::from(self.offset.get());
        start..start + usize::from(self.kind_and_key_len.get() & KEY_LEN_MASK)
    }

    /// Value length, excluding the reference bit.
    fn value_len(&self) -> usize {
        usize::from(self.ref_and_value_len.get() & VALUE_LEN_MASK)
    }
}

/// Shared two-bit record-kind encoding for base leaves and mini-pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordKind {
    /// A live value, or a buffered insertion in a mini-page.
    Insert = 0b00,
    /// A deletion awaiting reclamation or propagation.
    Delete = 0b01,
    /// A clean cached value.
    Cache = 0b10,
    /// A clean cached absence.
    Phantom = 0b11,
}

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

    /// Borrow the value for a nonempty key, or return `None` if it is absent.
    ///
    /// An empty value is present and returns `Some(&[])`.
    pub fn get(&self, key: &[u8]) -> Result<Option<&[u8]>, PageError> {
        if key.is_empty() {
            return Err(PageError::EmptyKey);
        }

        let index = self.lower_bound(key);
        if index == self.len() {
            return Ok(None);
        }

        let (stored_key, value) = self.record_at(index);
        Ok((stored_key == key).then_some(value))
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

    fn record_at(&self, index: usize) -> (&[u8], &[u8]) {
        let slot = self.slots()[index];
        let key = slot.key_range();
        let value = key.end..key.end + slot.value_len();
        let bytes = self.as_bytes();

        (&bytes[key], &bytes[value])
    }

    /// Index of the first slot whose key is not less than `key`.
    fn lower_bound(&self, key: &[u8]) -> usize {
        let bytes = self.as_bytes();

        self.slots()
            .partition_point(|slot| &bytes[slot.key_range()] < key)
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

    #[test]
    fn lookup_worked_byte_fixture() {
        let mut storage = [0; 128];
        let page = Page::init(&mut storage).unwrap();
        page.as_mut_bytes()[0..2].copy_from_slice(&[3, 0xa0]);
        // Free space: 113 - 32 header bytes - 3 slots * 8 bytes.
        page.as_mut_bytes()[4..6].copy_from_slice(&57u16.to_le_bytes());
        page.as_mut_bytes()[32..56].copy_from_slice(&[
            118, 0, 2, 0, 3, 0, b'1', b'0', // 10/dog
            113, 0, 2, 0, 3, 0, b'2', b'0', // 20/owl
            123, 0, 2, 0, 3, 0, b'3', b'0', // 30/cat
        ]);
        page.as_mut_bytes()[113..128].copy_from_slice(b"20owl10dog30cat");
        let before = page.as_bytes().to_vec();
        assert_eq!(page.len(), 3);
        assert!(!page.is_empty());
        assert_eq!(page.free_bytes(), 57);
        for (key, value) in [(b"10", b"dog"), (b"20", b"owl"), (b"30", b"cat")] {
            assert_eq!(page.get(key), Ok(Some(value.as_slice())));
        }
        assert_eq!(
            page.get(b"20").unwrap().unwrap().as_ptr(),
            page.as_bytes()[115..].as_ptr()
        );
        for key in [b"01".as_slice(), b"15", b"25", b"40", b"20\0"] {
            assert_eq!(page.get(key), Ok(None));
        }
        assert_eq!(page.get(b""), Err(PageError::EmptyKey));
        assert_eq!(page.as_bytes(), before);
    }

    #[test]
    fn lookup_prefixes_preview_collisions_and_empty_values() {
        let mut storage = [0; 128];
        let page = Page::init(&mut storage).unwrap();
        page.as_mut_bytes()[0..2].copy_from_slice(&[4, 0]);
        // Free space: 118 - 32 header bytes - 4 slots * 8 bytes.
        page.as_mut_bytes()[4..6].copy_from_slice(&54u16.to_le_bytes());
        page.as_mut_bytes()[32..64].copy_from_slice(&[
            118, 0, 1, 0, 1, 0, 0, 0, // \0/A
            120, 0, 2, 0, 0, 0, 0, 0, // \0\0/empty
            122, 0, 2, 0, 0, 0, b'a', b'b', // ab/empty
            124, 0, 3, 0, 1, 0, b'a', b'b', // abc/Z
        ]);
        page.as_mut_bytes()[118..128].copy_from_slice(b"\0A\0\0ababcZ");
        for (key, value) in [
            (b"\0".as_slice(), b"A".as_slice()),
            (b"\0\0", b""),
            (b"ab", b""),
            (b"abc", b"Z"),
        ] {
            assert_eq!(page.get(key), Ok(Some(value)));
        }
        for key in [b"\0\0\0".as_slice(), b"a", b"abb", b"abcd", b"\xff"] {
            assert_eq!(page.get(key), Ok(None));
        }
    }

    #[test]
    fn slot_lengths_exclude_all_kind_and_reference_bits() {
        for kind in [
            RecordKind::Insert,
            RecordKind::Delete,
            RecordKind::Cache,
            RecordKind::Phantom,
        ] {
            for referenced in [false, true] {
                let mut storage = [0; 128];
                let page = Page::init(&mut storage).unwrap();
                page.as_mut_bytes()[0..2].copy_from_slice(&[1, 0]);
                // Free space: 126 - 32 header bytes - 1 slot * 8 bytes.
                page.as_mut_bytes()[4..6].copy_from_slice(&86u16.to_le_bytes());
                let key_word = ((kind as u16) << 14) | 1;
                let value_word = (u16::from(referenced) << 15) | 1;
                page.as_mut_bytes()[32..40].copy_from_slice(&[
                    126,
                    0,
                    key_word as u8,
                    (key_word >> 8) as u8,
                    value_word as u8,
                    (value_word >> 8) as u8,
                    b'k',
                    0,
                ]);
                page.as_mut_bytes()[126..128].copy_from_slice(b"kV");
                let before = page.as_bytes().to_vec();
                assert_eq!(page.record_at(0), (b"k".as_slice(), b"V".as_slice()));
                assert_eq!(page.get(b"k"), Ok(Some(b"V".as_slice())));
                assert_eq!(page.as_bytes(), before);
            }
        }
    }

    #[test]
    fn lookup_empty_page_and_empty_key() {
        let mut storage = [0; 64];
        let page = Page::init(&mut storage).unwrap();
        let before = page.as_bytes().to_vec();
        assert_eq!(page.get(b"missing"), Ok(None));
        assert_eq!(page.get(b""), Err(PageError::EmptyKey));
        assert_eq!(page.as_bytes(), before);
    }
}
