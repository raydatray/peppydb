use zerocopy::little_endian::U64;
use zerocopy::{FromBytes, IntoBytes};

use super::layout::{
    CHILD_BYTES, HEADER_BYTES, SLOT_BYTES, Slot, encode_kind, entry_size, stored_parts,
};
use super::{InnerError, InnerNode, SeparatorRef};
use crate::ChildRef;

impl InnerNode {
    /// Insert a distinct, nonempty separator and the child to its right.
    ///
    /// The child left of the new separator, and every other entry, stay
    /// unchanged. Every error leaves all bytes unchanged.
    pub fn try_insert(&mut self, key: &[u8], right: ChildRef) -> Result<(), InnerError> {
        let needed = self.check_separator(key, right)?;
        let index = self
            .search(key)
            .err()
            .ok_or(InnerError::DuplicateSeparator)?;
        if needed > self.free_bytes() {
            return Err(InnerError::NodeFull);
        }

        self.insert_at(
            index,
            SeparatorRef {
                key: stored_parts(key),
                right_child: right,
            },
        );

        Ok(())
    }

    /// Replace one child and return the old one. Only its eight bytes change,
    /// so this succeeds on a full node. Errors leave all bytes unchanged.
    pub fn replace_child(&mut self, index: usize, child: ChildRef) -> Result<ChildRef, InnerError> {
        let old = self.child_at(index).ok_or(InnerError::InvalidChildIndex)?;
        self.check_kind(child)?;

        match index.checked_sub(1) {
            None => self.header.first_child.set(child.raw()),
            Some(separator) => {
                let start = self.slots()[separator].child_offset();
                let raw = U64::mut_from_bytes(&mut self.as_mut_bytes()[start..start + CHILD_BYTES])
                    .expect("child IDs are eight bytes");
                raw.set(child.raw());
            }
        }

        Ok(old)
    }

    /// Check a new separator's key and child; return its entry size.
    pub(super) fn check_separator(&self, key: &[u8], child: ChildRef) -> Result<usize, InnerError> {
        if key.is_empty() {
            return Err(InnerError::EmptyKey);
        }
        self.check_kind(child)?;

        // A slice is at most `isize::MAX` bytes, so the size cannot overflow.
        // The budget bounds the key length to the u16 slot field.
        let needed = entry_size(key.len());
        if needed > self.capacity().saturating_sub(HEADER_BYTES) {
            return Err(InnerError::SeparatorTooLarge);
        }

        Ok(needed)
    }

    fn check_kind(&self, child: ChildRef) -> Result<(), InnerError> {
        if encode_kind(child) != self.header.children_kind.get() {
            return Err(InnerError::ChildKindMismatch);
        }

        Ok(())
    }

    pub(super) fn search(&self, key: &[u8]) -> Result<usize, usize> {
        self.slots()
            .binary_search_by(|slot| self.key_of(slot).cmp_bytes(key))
    }

    /// Store `separator` at directory position `index`, shifting later slots.
    ///
    /// The caller must have checked order, kind, and space. The key's prefix
    /// must hold exactly its inline bytes, as from `stored_parts`.
    pub(super) fn insert_at(&mut self, index: usize, separator: SeparatorRef<'_>) {
        let count = self.len();
        let free = self.free_bytes();
        let tail = separator.key.suffix();
        let needed = entry_size(separator.key.len());

        // Capacity is at most 4096, which bounds the offset, key length, and
        // count to u16.
        let payload_start = HEADER_BYTES + count * SLOT_BYTES + free;
        let offset = payload_start - (needed - SLOT_BYTES);
        let child_start = offset + tail.len();

        let bytes = self.as_mut_bytes();
        bytes[offset..child_start].copy_from_slice(tail);
        bytes[child_start..payload_start]
            .copy_from_slice(&separator.right_child.raw().to_le_bytes());

        let (slots, _): (&mut [Slot], _) =
            FromBytes::mut_from_prefix_with_elems(&mut self.data, count + 1)
                .expect("preflight reserved space for the new slot");
        slots.copy_within(index..count, index + 1);
        slots[index] = Slot::new(offset as u16, separator.key);

        self.header.count.set((count + 1) as u16);
        self.header.remaining_size.set((free - needed) as u16);
    }
}
