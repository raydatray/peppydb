//! Packed pages backed by caller-owned storage.
//!
//! ```
//! use peppydb_bftree::Page;
//!
//! let mut storage = [0; 128];
//! let page = Page::init(&mut storage).unwrap();
//! page.try_insert(b"30", b"cat").unwrap();
//! page.try_insert(b"10", b"dog").unwrap();
//! page.try_insert(b"20", b"owl").unwrap();
//!
//! // Reads need only a shared reference.
//! let page: &Page = page;
//! assert_eq!(page.get(b"20"), Ok(Some(b"owl".as_slice())));
//! let records: Vec<_> = page.iter().collect();
//! assert_eq!(records, vec![
//!     (b"10".as_slice(), b"dog".as_slice()),
//!     (b"20".as_slice(), b"owl".as_slice()),
//!     (b"30".as_slice(), b"cat".as_slice()),
//! ]);
//! for (key, value) in page {
//!     assert_eq!(page.get(key), Ok(Some(value)));
//! }
//! ```

use std::iter::FusedIterator;
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
    /// Encode an unreferenced `Insert` record. Preflight must bound both lengths.
    fn insert(offset: u16, key: &[u8], value_len: u16) -> Self {
        Self {
            offset: U16::new(offset),
            kind_and_key_len: U16::new(RecordKind::Insert.key_word(key.len() as u16)),
            ref_and_value_len: U16::new(value_len),
            preview: [key[0], key.get(1).copied().unwrap_or(0)],
        }
    }

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

impl RecordKind {
    fn key_word(self, key_len: u16) -> u16 {
        ((self as u16) << 14) | key_len
    }
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

    /// Insert a distinct, nonempty key and its value, which may be empty.
    ///
    /// Validation rejects an empty key, then a record too large for an empty
    /// page, then a duplicate key, then insufficient current space. Every error
    /// leaves all bytes unchanged.
    pub fn try_insert(&mut self, key: &[u8], value: &[u8]) -> Result<(), PageError> {
        if key.is_empty() {
            return Err(PageError::EmptyKey);
        }

        let needed = key
            .len()
            .checked_add(value.len())
            .and_then(|size| size.checked_add(SLOT_BYTES))
            .filter(|&size| size <= self.capacity() - HEADER_BYTES)
            .ok_or(PageError::RecordTooLarge)?;
        let count = self.len();
        let index = self.lower_bound(key);
        if index < count && self.key_at(index) == key {
            return Err(PageError::DuplicateKey);
        }

        let free = self.free_bytes();
        if needed > free {
            return Err(PageError::PageFull);
        }

        // Capacity is at most 4096: preflight also bounds lengths and offsets
        // to their slot bit fields, and the count to fewer than 512 records.
        let payload_start = HEADER_BYTES + count * SLOT_BYTES + free;
        let offset = payload_start - (needed - SLOT_BYTES);
        let value_start = offset + key.len();

        let bytes = self.as_mut_bytes();
        bytes[offset..value_start].copy_from_slice(key);
        bytes[value_start..payload_start].copy_from_slice(value);

        let (slots, _): (&mut [Slot], _) =
            FromBytes::mut_from_prefix_with_elems(&mut self.data, count + 1)
                .expect("preflight reserved space for the new slot");
        slots.copy_within(index..count, index + 1);
        slots[index] = Slot::insert(offset as u16, key, value.len() as u16);

        let flags = self.header.count_and_flags.get() & !COUNT_MASK;
        self.header.count_and_flags.set(flags | (count + 1) as u16);
        self.header.remaining_size.set((free - needed) as u16);

        Ok(())
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

    /// Borrow key/value pairs in lexicographic key order without allocation.
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            page: self,
            indices: 0..self.len(),
        }
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

    /// Borrow the key addressed by slot `index` without decoding its value.
    fn key_at(&self, index: usize) -> &[u8] {
        &self.as_bytes()[self.slots()[index].key_range()]
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

impl<'p> IntoIterator for &'p Page {
    type Item = (&'p [u8], &'p [u8]);
    type IntoIter = Iter<'p>;

    fn into_iter(self) -> Iter<'p> {
        self.iter()
    }
}

/// Borrowed key/value pairs of a [`Page`] in lexicographic key order.
///
/// Created by [`Page::iter`] or by iterating over `&Page`.
#[derive(Clone, Debug)]
pub struct Iter<'p> {
    page: &'p Page,
    indices: Range<usize>,
}

impl<'p> Iterator for Iter<'p> {
    type Item = (&'p [u8], &'p [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        self.indices.next().map(|index| self.page.record_at(index))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.indices.size_hint()
    }
}

impl ExactSizeIterator for Iter<'_> {}

impl FusedIterator for Iter<'_> {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    // Tests decode raw byte offsets rather than `Header` and `Slot`, so they
    // check the structs against the documented format independently.
    fn read_u16(bytes: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
    }

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

    fn assert_rejected(page: &mut Page, key: &[u8], value: &[u8], error: PageError) {
        let before = page.as_bytes().to_vec();
        assert_eq!(page.try_insert(key, value), Err(error));
        assert_eq!(page.as_bytes(), before);
    }

    #[test]
    fn insert_worked_example_matches_golden_bytes() {
        let mut storage = [0; 128];
        let page = Page::init(&mut storage).unwrap();
        page.try_insert(b"30", b"cat").unwrap();
        let first_payload = page.as_bytes()[123..].to_vec();
        page.try_insert(b"10", b"dog").unwrap();
        assert_eq!(&page.as_bytes()[123..], first_payload);
        assert_eq!(page.free_bytes(), 70);
        let old_payloads = page.as_bytes()[118..].to_vec();
        page.try_insert(b"20", b"owl").unwrap();
        assert_eq!(&page.as_bytes()[118..], old_payloads);
        assert_eq!(page.len(), 3);
        assert_eq!(page.free_bytes(), 57);
        assert_eq!(32 + page.len() * 8 + page.free_bytes(), 113);
        assert_eq!(
            &page.as_bytes()[32..56],
            &[
                118, 0, 2, 0, 3, 0, b'1', b'0', 113, 0, 2, 0, 3, 0, b'2', b'0', 123, 0, 2, 0, 3, 0,
                b'3', b'0',
            ]
        );
        assert_eq!(&page.as_bytes()[113..], b"20owl10dog30cat");
        for (key, value) in [(b"10", b"dog"), (b"20", b"owl"), (b"30", b"cat")] {
            assert_eq!(page.get(key), Ok(Some(value.as_slice())));
        }
        assert_eq!(page.get(b"25"), Ok(None));
    }

    #[test]
    fn insert_prefixes_zero_bytes_and_empty_values() {
        let keys = [b"abc".as_slice(), b"\0\0", b"ab", b"\0", b"\xff", b"a"];
        let sorted = [b"\0".as_slice(), b"\0\0", b"a", b"ab", b"abc", b"\xff"];
        let mut storage = [0; 128];
        let page = Page::init(&mut storage).unwrap();
        for key in keys {
            page.try_insert(key, b"").unwrap();
            assert_eq!(page.get(key), Ok(Some(b"".as_slice())));
        }
        for (index, key) in sorted.into_iter().enumerate() {
            assert_eq!(page.record_at(index), (key, b"".as_slice()));
            let slot = 32 + index * 8;
            assert_eq!(read_u16(page.as_bytes(), slot + 2), key.len() as u16);
            assert_eq!(read_u16(page.as_bytes(), slot + 4), 0);
            assert_eq!(
                &page.as_bytes()[slot + 6..slot + 8],
                &[key[0], key.get(1).copied().unwrap_or(0)]
            );
        }
        assert_rejected(page, b"ab", b"new", PageError::DuplicateKey);
    }

    #[test]
    fn exact_fits_and_error_precedence_preserve_bytes() {
        let mut storage = [0; 64];
        let page = Page::init(&mut storage).unwrap();
        // Exact fit: 32 header bytes + 8 slot bytes + 1 key byte + 23 value bytes.
        let value = vec![0xa5; 23];
        let too_large = vec![0xa5; value.len() + 1];
        assert_rejected(page, b"", &too_large, PageError::EmptyKey);
        assert_rejected(page, b"k", &too_large, PageError::RecordTooLarge);
        assert_rejected(page, &vec![0; 0x4000], b"", PageError::RecordTooLarge);
        assert_rejected(page, b"k", &vec![0; 0x8000], PageError::RecordTooLarge);
        page.try_insert(b"k", &value).unwrap();
        assert_eq!(page.free_bytes(), 0);
        assert_eq!(page.len(), 1);
        assert_eq!(page.get(b"k"), Ok(Some(value.as_slice())));
        assert_rejected(page, b"", &too_large, PageError::EmptyKey);
        assert_rejected(page, b"k", &too_large, PageError::RecordTooLarge);
        assert_rejected(page, b"k", b"", PageError::DuplicateKey);
        assert_rejected(page, b"x", b"", PageError::PageFull);
    }

    #[test]
    fn insertion_preserves_flags_and_inactive_metadata() {
        let mut storage = [0; 128];
        let page = Page::init(&mut storage).unwrap();
        page.as_mut_bytes()[0..2].copy_from_slice(&0xb000u16.to_le_bytes());
        page.as_mut_bytes()[6..32].fill(0xa5);
        let inactive = page.as_bytes()[6..32].to_vec();
        for (index, key) in [b"30", b"10", b"20"].into_iter().enumerate() {
            page.try_insert(key, b"value").unwrap();
            assert_eq!(page.len(), index + 1);
            assert_eq!(read_u16(page.as_bytes(), 0), 0xb000 | (index + 1) as u16);
            assert_eq!(&page.as_bytes()[6..32], inactive);
            assert_eq!(page.capacity(), 128);
        }
    }

    #[test]
    fn iterate_empty_page() {
        let mut storage = [0; 64];
        let page: &Page = Page::init(&mut storage).unwrap();
        let mut iter = page.iter();
        assert_eq!(iter.len(), 0);
        assert_eq!(iter.next(), None);
        assert_eq!(iter.next(), None);
        assert_eq!(IntoIterator::into_iter(page).count(), 0);
    }

    #[test]
    fn iterate_borrowed_records_in_key_order() {
        let records = [
            (b"\0".as_slice(), b"".as_slice()),
            (b"\0\0", b"zero"),
            (b"ab", b"short"),
            (b"abc", b""),
            (b"abcd", b"long"),
            (b"\xff", b"\0"),
        ];
        for order in [[0, 1, 2, 3, 4, 5], [5, 4, 3, 2, 1, 0], [2, 5, 1, 4, 0, 3]] {
            let mut storage = [0; 128];
            let page = Page::init(&mut storage).unwrap();
            for index in order {
                let (key, value) = records[index];
                page.try_insert(key, value).unwrap();
            }
            let page: &Page = page;
            let before = page.as_bytes().to_vec();
            let mut iter = page.iter();
            for (index, &(key, value)) in records.iter().enumerate() {
                assert_eq!(iter.len(), records.len() - index);
                let actual = iter.next().unwrap();
                assert_eq!(actual, (key, value));
                let offset = usize::from(read_u16(page.as_bytes(), 32 + index * 8));
                assert_eq!(actual.0.as_ptr(), page.as_bytes()[offset..].as_ptr());
                assert_eq!(actual.1.as_ptr(), page.get(key).unwrap().unwrap().as_ptr());
            }
            assert_eq!(iter.len(), 0);
            assert_eq!(iter.next(), None);
            assert_eq!(iter.next(), None);
            let mut expected = records.iter().copied();
            for record in page {
                assert_eq!(Some(record), expected.next());
            }
            assert_eq!(expected.next(), None);
            assert_eq!(page.as_bytes(), before);
        }
    }

    fn assert_model(page: &Page, model: &BTreeMap<Vec<u8>, Vec<u8>>) {
        let before = page.as_bytes().to_vec();
        let capacity = before.len();
        let payload_bytes: usize = model
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum();
        let slots_end = 32 + model.len() * 8;
        let payload_start = capacity - payload_bytes;
        assert_eq!(page.len(), model.len());
        assert_eq!(page.is_empty(), model.is_empty());
        assert_eq!(page.capacity(), capacity);
        assert_eq!(page.free_bytes(), payload_start - slots_end);
        let actual: Vec<_> = page.iter().collect();
        let expected: Vec<_> = model
            .iter()
            .map(|(key, value)| (key.as_slice(), value.as_slice()))
            .collect();
        assert_eq!(actual, expected);
        for (key, value) in model {
            assert_eq!(page.get(key), Ok(Some(value.as_slice())));
        }
        assert_eq!(page.get(b""), Err(PageError::EmptyKey));
        assert_eq!(page.get(b"not-present"), Ok(None));
        assert_eq!(page.get(b"\xff\xff"), Ok(None));

        // Decode independently of the production structs. State bits must be
        // zero for every record this API can create.
        let bytes = page.as_bytes();
        let word = |offset| u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
        assert_eq!(word(0), model.len() as u16);
        assert_eq!(word(2), capacity as u16);
        assert_eq!(usize::from(word(4)), payload_start - slots_end);
        assert_eq!(&bytes[6..8], &[0; 2]);
        assert_eq!(&bytes[8..16], &[0xff; 8]);
        assert_eq!(&bytes[16..32], &[0; 16]);
        let mut occupied = vec![false; capacity];
        occupied[..slots_end].fill(true);
        for (index, (key, value)) in model.iter().enumerate() {
            let slot = 32 + index * 8;
            let offset = usize::from(word(slot));
            let value_start = offset + key.len();
            let end = value_start + value.len();
            assert!(offset >= payload_start && end <= capacity);
            assert_eq!(word(slot + 2), key.len() as u16);
            assert_eq!(word(slot + 4), value.len() as u16);
            assert_eq!(
                &bytes[slot + 6..slot + 8],
                &[key[0], key.get(1).copied().unwrap_or(0)]
            );
            assert_eq!(&bytes[offset..value_start], key);
            assert_eq!(&bytes[value_start..end], value);
            assert!(occupied[offset..end].iter().all(|&used| !used));
            occupied[offset..end].fill(true);
        }
        assert!(occupied[slots_end..payload_start].iter().all(|&used| !used));
        assert!(occupied[payload_start..].iter().all(|&used| used));
        assert_eq!(page.as_bytes(), before);
    }

    fn model_insert(
        page: &mut Page,
        model: &mut BTreeMap<Vec<u8>, Vec<u8>>,
        key: &[u8],
        value: &[u8],
    ) {
        let usable = page.as_bytes().len() - 32;
        let required = 8 + key.len() + value.len();
        let used: usize = model
            .iter()
            .map(|(key, value)| 8 + key.len() + value.len())
            .sum();
        let expected = if key.is_empty() {
            Err(PageError::EmptyKey)
        } else if required > usable {
            Err(PageError::RecordTooLarge)
        } else if model.contains_key(key) {
            Err(PageError::DuplicateKey)
        } else if used + required > usable {
            Err(PageError::PageFull)
        } else {
            Ok(())
        };
        let before = page.as_bytes().to_vec();
        assert_eq!(page.try_insert(key, value), expected);
        if expected.is_ok() {
            model.insert(key.to_vec(), value.to_vec());
        } else {
            assert_eq!(page.as_bytes(), before);
        }
        if !key.is_empty() {
            assert_eq!(page.get(key), Ok(model.get(key).map(Vec::as_slice)));
        }
        assert_model(page, model);
    }

    #[test]
    fn deterministic_sequences_match_ordered_map() {
        // Powers of two, upstream's default mini-page sizes that are not
        // powers of two, and 4032, the largest such supported capacity.
        for capacity in [64, 128, 256, 448, 512, 832, 1024, 1600, 2048, 4032, 4096] {
            let count = (capacity / 10 + 16).min(420);
            let mut keys: Vec<_> = (0..count as u16)
                .map(|index| index.to_be_bytes().to_vec())
                .collect();
            keys.extend([
                b"\0".to_vec(),
                b"\0\0\0".to_vec(),
                b"ab".to_vec(),
                b"abc".to_vec(),
                b"\xff".to_vec(),
            ]);
            keys.sort();
            for order in 0..3 {
                let mut indices: Vec<_> = (0..keys.len()).collect();
                match order {
                    0 => {}
                    1 => indices.reverse(),
                    _ => indices
                        .sort_unstable_by_key(|&index| (index as u32).wrapping_mul(0x9e37_79b9)),
                }
                for profile in 0..2 {
                    let mut storage = vec![0xa5; capacity];
                    let page = Page::init(&mut storage).unwrap();
                    let mut model = BTreeMap::new();
                    assert_model(page, &model);
                    model_insert(page, &mut model, b"", b"");
                    for &index in &indices {
                        let value_len = if profile == 0 {
                            0
                        } else {
                            [0, 1, 2, 7, 31, capacity / 4, capacity][index % 7]
                        };
                        let value: Vec<_> =
                            (0..value_len).map(|byte| (byte ^ index) as u8).collect();
                        model_insert(page, &mut model, &keys[index], &value);
                        if index % 11 == 0 {
                            model_insert(page, &mut model, &keys[index], b"replacement");
                        }
                    }
                    let oversized = vec![0; capacity];
                    model_insert(page, &mut model, &keys[0], &oversized);
                    model_insert(page, &mut model, &oversized, b"");
                    model_insert(page, &mut model, b"", &oversized);
                    if profile == 0 && capacity == 4096 {
                        assert!(model.len() > 255, "exercise both count bytes");
                    }
                }
            }
        }
    }
}
