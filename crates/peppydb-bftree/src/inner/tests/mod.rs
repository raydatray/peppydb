use super::*;
use crate::{InnerId, LeafId};

mod layout;
mod read;

// Tests decode raw byte offsets rather than `Header` and `Slot`, so they
// check the structs against the documented format independently.
fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn leaf(raw: u64) -> ChildRef {
    ChildRef::Leaf(LeafId::new(raw).unwrap())
}

fn inner(raw: u64) -> ChildRef {
    ChildRef::Inner(InnerId::new(raw).unwrap())
}
