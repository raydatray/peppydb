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

/// Logical state of a stored record. Empty live values are distinct from absence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordState<'a> {
    /// A live value, or buffered insertion.
    Insert(&'a [u8]),
    /// A buffered deletion; retained physical value bytes are not exposed.
    Delete,
    /// A clean cached value.
    Cache(&'a [u8]),
    /// A clean cached absence.
    Phantom,
}

impl RecordState<'_> {
    /// Encoded kind corresponding to this state.
    pub fn kind(self) -> RecordKind {
        match self {
            Self::Insert(_) => RecordKind::Insert,
            Self::Delete => RecordKind::Delete,
            Self::Cache(_) => RecordKind::Cache,
            Self::Phantom => RecordKind::Phantom,
        }
    }
}

impl<'a> RecordState<'a> {
    /// Borrow the logical value, if this is a live record.
    pub fn value(self) -> Option<&'a [u8]> {
        match self {
            Self::Insert(value) | Self::Cache(value) => Some(value),
            Self::Delete | Self::Phantom => None,
        }
    }
}

/// A record borrowed from a page, including its exact state and reference hint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordRef<'a> {
    pub key: &'a [u8],
    pub state: RecordState<'a>,
    pub referenced: bool,
}
