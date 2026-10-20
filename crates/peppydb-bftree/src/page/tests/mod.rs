use std::collections::BTreeMap;

use super::*;

mod layout;
mod model;
mod read;
mod write;

// Tests decode raw byte offsets rather than `Header` and `Slot`, so they
// check the structs against the documented format independently.
fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn live<'a>(key: &'a [u8], value: &'a [u8]) -> RecordRef<'a> {
    RecordRef {
        key,
        state: RecordState::Insert(value),
        referenced: false,
    }
}
