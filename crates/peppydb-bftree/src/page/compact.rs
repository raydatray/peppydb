use zerocopy::{FromBytes, IntoBytes};

use super::Page;
use super::layout::{HEADER_BYTES, SLOT_BYTES};

impl Page {
    /// Unreachable payload bytes, excluding the contiguous free gap.
    /// Retained tombstone value allocations remain reachable.
    pub fn reclaimable_bytes(&self) -> usize {
        let reachable: usize = self
            .slots()
            .iter()
            .map(|slot| slot.key_range().len() + slot.value_len())
            .sum();
        self.capacity() - HEADER_BYTES - self.len() * SLOT_BYTES - reachable - self.free_bytes()
    }

    /// Pack every stored payload and return the bytes recovered into the gap.
    ///
    /// Preserves all record states, reference bits, retained tombstone bytes, and
    /// header metadata. Uses one bounded 4096-byte scratch image, without heap
    /// allocation. No pending write is applied and no records are discarded.
    pub fn compact(&mut self) -> usize {
        let recovered = self.reclaimable_bytes();
        let capacity = self.capacity();
        let mut scratch = [0; 4096];
        scratch[..HEADER_BYTES].copy_from_slice(&self.as_bytes()[..HEADER_BYTES]);
        let destination = Page::mut_from_bytes(&mut scratch[..capacity])
            .expect("scratch image has the source capacity");
        self.pack_payloads_into(destination);
        self.as_mut_bytes().copy_from_slice(destination.as_bytes());
        recovered
    }

    /// Rebuild physical slots, not logical records: absence views hide values.
    /// The destination already has the source header and the same capacity.
    fn pack_payloads_into(&self, destination: &mut Page) {
        let mut end = self.capacity();
        for (index, slot) in self.slots().iter().enumerate() {
            let start = slot.key_range().start;
            let length = slot.key_range().len() + slot.value_len();
            end -= length;
            destination.as_mut_bytes()[end..end + length]
                .copy_from_slice(&self.as_bytes()[start..start + length]);
            let mut packed = *slot;
            packed.offset.set(end as u16);
            destination.slots_mut()[index] = packed;
        }
        destination
            .header
            .remaining_size
            .set((end - HEADER_BYTES - self.len() * SLOT_BYTES) as u16);
    }
}
