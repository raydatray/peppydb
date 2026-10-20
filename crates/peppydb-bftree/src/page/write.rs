use zerocopy::{FromBytes, IntoBytes};

use super::layout::{COUNT_MASK, HEADER_BYTES, SLOT_BYTES, Slot};
use super::{Page, PageError, RecordState};

impl Page {
    /// Insert a distinct, nonempty key and its value, which may be empty.
    ///
    /// Validation rejects an empty key, then a record too large for an empty
    /// page, then a duplicate key, then insufficient current space. Every error
    /// leaves all bytes unchanged.
    pub fn try_insert(&mut self, key: &[u8], value: &[u8]) -> Result<(), PageError> {
        self.try_insert_record(key, RecordState::Insert(value))
    }

    /// Insert a distinct key with any record state. Absence states store no value.
    ///
    /// Validation and byte-preserving failure follow [`Page::try_insert`].
    /// New records start with a clear reference bit.
    pub fn try_insert_record(
        &mut self,
        key: &[u8],
        state: RecordState<'_>,
    ) -> Result<(), PageError> {
        if key.is_empty() {
            return Err(PageError::EmptyKey);
        }

        let value = state.value().unwrap_or_default();
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
        slots[index] = Slot::new(offset as u16, key, value.len() as u16, state.kind());

        let flags = self.header.count_and_flags.get() & !COUNT_MASK;
        self.header.count_and_flags.set(flags | (count + 1) as u16);
        self.header.remaining_size.set((free - needed) as u16);

        Ok(())
    }

    /// Set or clear the reference bit; return whether the key has a stored entry.
    ///
    /// Requires a nonempty key. No bytes change for a missing key or an error.
    pub fn set_referenced(&mut self, key: &[u8], referenced: bool) -> Result<bool, PageError> {
        if key.is_empty() {
            return Err(PageError::EmptyKey);
        }
        let index = self.lower_bound(key);
        if index == self.len() || self.key_at(index) != key {
            return Ok(false);
        }
        self.slots_mut()[index].set_referenced(referenced);
        Ok(true)
    }
}
