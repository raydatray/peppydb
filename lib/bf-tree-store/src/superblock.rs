//! The superblock codec: configuration and committed state in one
//! checksummed copy (superblock version 2, plan 0017).

use zerocopy::little_endian::{U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::StoreError;
use bf_tree_page::Page;

/// Magic bytes at the start of every superblock copy.
pub const MAGIC: [u8; 8] = *b"PEPPYBF\0";
/// The only superblock layout this crate reads and writes.
pub const VERSION: u32 = 2;
/// Bytes of one superblock copy, whatever the page size.
pub const COPY_BYTES: usize = 4096;
/// Byte offset of page number 0: after two superblock copies.
pub const PAGES_OFFSET: u64 = 2 * COPY_BYTES as u64;

/// The store's self-description: page size and the tree configuration that
/// every stored page was written under.
///
/// The configuration fixes the admission rule for the file. A tree opened
/// from the store must use it, so a reopened tree never admits a key the
/// stored pages cannot split.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Superblock {
    /// Page length and leaf page capacity.
    pub page_size: usize,
    pub inner_capacity: usize,
    pub max_key_len: usize,
    pub max_value_len: usize,
}

/// The committed checkpoint the store's pages reflect. All zero for a new
/// store. Plan 0018 gives `applied_seq` and `write_seq` their meaning.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub checkpoint_id: u64,
    pub applied_seq: u64,
    pub write_seq: u64,
}

/// On-disk superblock copy, version 2. Little-endian, zero-padded to the
/// copy's length. `crc` is the CRC32C of every byte before it.
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
struct EncodedSuperblock {
    magic: [u8; 8],
    version: U32,
    page_size: U32,
    inner_capacity: U32,
    max_key_len: U32,
    max_value_len: U32,
    checkpoint_id: U64,
    applied_seq: U64,
    write_seq: U64,
    crc: U32,
}

/// Encoded bytes of one copy, before the zero padding.
pub(crate) const ENCODED_BYTES: usize = size_of::<EncodedSuperblock>();
const _: () = assert!(ENCODED_BYTES == 56);
const CRC_START: usize = ENCODED_BYTES - 4;

impl Superblock {
    /// Check that the page size is a legal page capacity and that every
    /// field fits its encoding.
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

    /// Write one copy holding this configuration and `state` into `block`,
    /// zero-padding the rest. `block` holds at least the encoded bytes.
    pub fn encode(&self, state: &State, block: &mut [u8]) -> Result<(), StoreError> {
        self.validate()?;
        if block.len() < ENCODED_BYTES {
            return Err(StoreError::BufferLength {
                expected: ENCODED_BYTES,
                actual: block.len(),
            });
        }

        block.fill(0);
        let mut encoded = EncodedSuperblock {
            magic: MAGIC,
            version: U32::new(VERSION),
            page_size: U32::new(self.page_size as u32),
            inner_capacity: U32::new(self.inner_capacity as u32),
            max_key_len: U32::new(self.max_key_len as u32),
            max_value_len: U32::new(self.max_value_len as u32),
            checkpoint_id: U64::new(state.checkpoint_id),
            applied_seq: U64::new(state.applied_seq),
            write_seq: U64::new(state.write_seq),
            crc: U32::ZERO,
        };
        encoded
            .crc
            .set(crc32c::crc32c(&encoded.as_bytes()[..CRC_START]));
        block[..ENCODED_BYTES].copy_from_slice(encoded.as_bytes());

        Ok(())
    }

    /// Decode one copy. Trailing bytes after the encoded fields are ignored.
    /// Rejects a bad magic, another version, a checksum mismatch, and an
    /// invalid configuration.
    pub fn decode(block: &[u8]) -> Result<(Self, State), StoreError> {
        let (encoded, _) = EncodedSuperblock::ref_from_prefix(block)
            .map_err(|_| StoreError::InvalidSuperblock("too short"))?;

        if encoded.magic != MAGIC {
            return Err(StoreError::InvalidSuperblock("bad magic"));
        }
        if encoded.version.get() != VERSION {
            return Err(StoreError::InvalidSuperblock("unsupported version"));
        }
        if encoded.crc.get() != crc32c::crc32c(&encoded.as_bytes()[..CRC_START]) {
            return Err(StoreError::InvalidSuperblock("checksum mismatch"));
        }

        let superblock = Self {
            page_size: encoded.page_size.get() as usize,
            inner_capacity: encoded.inner_capacity.get() as usize,
            max_key_len: encoded.max_key_len.get() as usize,
            max_value_len: encoded.max_value_len.get() as usize,
        };
        superblock.validate()?;
        let state = State {
            checkpoint_id: encoded.checkpoint_id.get(),
            applied_seq: encoded.applied_seq.get(),
            write_seq: encoded.write_seq.get(),
        };

        Ok((superblock, state))
    }

    /// Choose between the two copies of a store. A copy that fails to decode
    /// is skipped, because a commit may have torn it. Two intact copies must
    /// describe the same configuration, because a commit never changes it;
    /// the copy with the higher checkpoint id wins.
    pub fn select(copy0: &[u8], copy1: &[u8]) -> Result<(Self, State), StoreError> {
        match (Self::decode(copy0), Self::decode(copy1)) {
            (Ok(a), Ok(b)) if a.0 != b.0 => Err(StoreError::InvalidSuperblock(
                "superblock copies disagree on the configuration",
            )),
            (Ok(a), Ok(b)) => Ok(if b.1.checkpoint_id > a.1.checkpoint_id {
                b
            } else {
                a
            }),
            (Ok(one), Err(_)) | (Err(_), Ok(one)) => Ok(one),
            (Err(_), Err(_)) => Err(StoreError::NoSuperblock),
        }
    }
}
