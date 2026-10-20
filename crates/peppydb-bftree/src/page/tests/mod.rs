use std::collections::BTreeMap;

use super::*;

mod compact;
mod fence;
mod key;
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
        key: KeyRef::from(key),
        state: RecordState::Insert(value),
        referenced: false,
    }
}

/// View full-key input bounds as the logical bounds a page should report.
fn logical<'a>(fences: Fences<&'a [u8]>) -> Fences<KeyRef<'a>> {
    let view = |fence| match fence {
        Fence::Unbounded => Fence::Unbounded,
        Fence::Key(key) => Fence::Key(KeyRef::from(key)),
    };
    Fences {
        lower: view(fences.lower),
        upper: view(fences.upper),
    }
}
