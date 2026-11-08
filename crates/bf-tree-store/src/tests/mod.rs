use super::*;

mod contract;
mod crc;
mod direct;
mod file;
mod superblock;

/// A small configuration shared by the store tests.
pub(super) fn superblock(page_size: usize) -> Superblock {
    Superblock {
        page_size,
        inner_capacity: 128,
        max_key_len: 8,
        max_value_len: 16,
    }
}

pub(super) fn page(raw: u64) -> PageNumber {
    PageNumber::new(raw).unwrap()
}

/// A page image whose header carries a nonzero capacity, so that no backend
/// mistakes it for a free page.
pub(super) fn page_image(page_size: usize, fill: u8) -> Vec<u8> {
    let mut page = vec![fill; page_size];
    page[2..4].copy_from_slice(&(page_size as u16).to_le_bytes());
    page
}
