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
    pub(super) fn key_word(self, key_len: u16) -> u16 {
        ((self as u16) << 14) | key_len
    }
}
