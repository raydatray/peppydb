use zerocopy::{FromBytes, IntoBytes};

use super::layout::{COUNT_MASK, HEADER_BYTES, SLOT_BYTES, Slot};
use super::{Page, PageError, RecordState};

impl Page {
    /// Insert a distinct, nonempty key and its value, which may be empty.
    ///
    /// Validation rejects an empty key, an out-of-range key, a record too large
    /// for an empty page with these fences, a duplicate key, then insufficient
    /// current space, in that order. Every error leaves all bytes unchanged.
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
        let needed = self.record_size(key, state)?;
        let count = self.len();
        let index = self.lower_bound(key);
        if index < count && self.key_at(index) == key {
            return Err(PageError::DuplicateKey);
        }
        self.insert_at(index, key, state, needed)
    }

    /// Insert or replace using existing storage and the contiguous free gap.
    ///
    /// Deleting an existing entry changes only its kind and retains its value
    /// allocation. Smaller replacements leave holes; growing ones allocate a
    /// complete new payload. Replacement preserves the reference bit. No path
    /// compacts implicitly, and every error preserves the complete page image.
    pub fn try_upsert(
        &mut self,
        key: &[u8],
        state: RecordState<'_>,
    ) -> Result<UpsertOutcome, PageError> {
        let needed = self.record_size(key, state)?;
        let index = self.lower_bound(key);
        if index == self.len() || self.key_at(index) != key {
            self.insert_at(index, key, state, needed)?;
            return Ok(UpsertOutcome::Inserted);
        }
        let index = index + self.record_start();
        let mut slot = self.slots()[index];
        if state != RecordState::Delete {
            let value = state.value().unwrap_or_default();
            if value.len() <= slot.value_len() {
                let start = slot.key_range().end;
                self.as_mut_bytes()[start..start + value.len()].copy_from_slice(value);
                slot.set_value_len(value.len() as u16);
            } else {
                let payload_len = needed - SLOT_BYTES;
                let free = self.free_bytes();
                if payload_len > free {
                    return Err(PageError::PageFull);
                }
                let end = HEADER_BYTES + self.slot_count() * SLOT_BYTES + free;
                let offset = end - payload_len;
                self.as_mut_bytes()[offset..offset + key.len()].copy_from_slice(key);
                self.as_mut_bytes()[offset + key.len()..end].copy_from_slice(value);
                slot.offset.set(offset as u16);
                slot.set_value_len(value.len() as u16);
                self.header.remaining_size.set((free - payload_len) as u16);
            }
        }
        slot.set_kind(state.kind());
        self.slots_mut()[index] = slot;
        Ok(UpsertOutcome::Replaced)
    }

    fn record_size(&self, key: &[u8], state: RecordState<'_>) -> Result<usize, PageError> {
        self.check_key(key)?;
        key.len()
            .checked_add(state.value().map_or(0, <[u8]>::len))
            .and_then(|size| size.checked_add(SLOT_BYTES))
            .filter(|&size| size <= self.record_capacity())
            .ok_or(PageError::RecordTooLarge)
    }

    fn insert_at(
        &mut self,
        index: usize,
        key: &[u8],
        state: RecordState<'_>,
        needed: usize,
    ) -> Result<(), PageError> {
        let count = self.slot_count();
        let index = index + self.record_start();
        let value = state.value().unwrap_or_default();

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
        self.check_key(key)?;
        let index = self.lower_bound(key);
        if index == self.len() || self.key_at(index) != key {
            return Ok(false);
        }
        let index = index + self.record_start();
        self.slots_mut()[index].set_referenced(referenced);
        Ok(true)
    }

    /// Physically forget a stored entry of any kind; return whether it existed.
    ///
    /// This is not a logical deletion: callers must retain unapplied tombstones.
    /// Its slot becomes free immediately; its payload becomes reclaimable space.
    pub fn remove(&mut self, key: &[u8]) -> Result<bool, PageError> {
        self.check_key(key)?;
        let index = self.lower_bound(key);
        if index == self.len() || self.key_at(index) != key {
            return Ok(false);
        }
        let index = index + self.record_start();
        let count = self.slot_count();
        self.slots_mut().copy_within(index + 1..count, index);
        let flags = self.header.count_and_flags.get() & !COUNT_MASK;
        self.header.count_and_flags.set(flags | (count - 1) as u16);
        self.header
            .remaining_size
            .set((self.free_bytes() + SLOT_BYTES) as u16);
        Ok(true)
    }
}

/// Whether a successful upsert created a slot or replaced an existing record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpsertOutcome {
    Inserted,
    Replaced,
}
