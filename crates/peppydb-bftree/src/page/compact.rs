use zerocopy::{FromBytes, IntoBytes};

use super::Page;
use super::layout::{HEADER_BYTES, SLOT_BYTES};

impl Page {
    /// Unreachable payload bytes, excluding the contiguous free gap.
    /// Retained tombstone value allocations remain reachable.
    pub fn reclaimable_bytes(&self) -> usize {
        let reachable: usize = (0..self.slot_count())
            .filter_map(|index| self.payload_range(index))
            .map(|payload| payload.len())
            .sum();

        self.capacity()
            - HEADER_BYTES
            - self.slot_count() * SLOT_BYTES
            - reachable
            - self.free_bytes()
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

    /// Copy raw slots and payloads rather than records from `iter()`, which hide
    /// tombstone values and include the prefix in keys. `destination` must
    /// already hold this page's header.
    fn pack_payloads_into(&self, destination: &mut Page) {
        let mut end = self.capacity();

        for (index, slot) in self.slots().iter().enumerate() {
            let mut packed = *slot;

            if let Some(payload) = self.payload_range(index) {
                end -= payload.len();
                destination.as_mut_bytes()[end..end + payload.len()]
                    .copy_from_slice(&self.as_bytes()[payload]);

                packed.offset.set(end as u16);
            }

            destination.slots_mut()[index] = packed;
        }

        destination
            .header
            .remaining_size
            .set((end - HEADER_BYTES - self.slot_count() * SLOT_BYTES) as u16);
    }
}
