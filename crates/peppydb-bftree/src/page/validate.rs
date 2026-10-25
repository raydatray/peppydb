use zerocopy::FromBytes;

use super::layout::{
    COUNT_MASK, HAS_FENCES, HEADER_BYTES, HIGH_INFINITY, LOW_INFINITY, SLOT_BYTES, Slot,
};
use super::{Fence, Page, PageError};

impl Page {
    /// Check bytes of unknown provenance and view them as a page.
    ///
    /// This is the single entry point for stored bytes. It never panics or
    /// reads outside `bytes`, and it accepts exactly the images that this
    /// crate's page operations produce, fenced or not:
    ///
    /// - the capacity is legal and equals the buffer length;
    /// - only known flags are set, and the slot directory and free gap fit;
    /// - every payload lies after the free gap, inside the buffer, and
    ///   payloads do not overlap;
    /// - fences, if present, are well formed, ordered, and consistent with
    ///   the stored prefix length;
    /// - record keys are nonempty, strictly increasing, inside the fences,
    ///   and their slot previews match the stored bytes.
    ///
    /// The returned page keeps the ordinary trusted-bytes contract, so every
    /// other method may rely on these checks. It costs one pass over the slot
    /// directory plus a sort of the payload ranges.
    pub fn validate(bytes: &[u8]) -> Result<&Self, PageError> {
        let corrupt = PageError::Corrupt;
        let capacity = bytes.len();
        Self::check_capacity(capacity).map_err(|_| corrupt("illegal capacity"))?;

        let page = Self::ref_from_bytes(bytes).map_err(|_| corrupt("header does not fit"))?;
        if page.capacity() != capacity {
            return Err(corrupt("capacity field differs from the buffer length"));
        }

        let flags = page.header.count_and_flags.get() & !COUNT_MASK;
        if flags & !HAS_FENCES != 0 {
            return Err(corrupt("unknown header flags"));
        }

        let count = page.slot_count();
        let directory_end = HEADER_BYTES + count * SLOT_BYTES;
        if directory_end > capacity {
            return Err(corrupt("slot directory does not fit"));
        }

        let gap_end = directory_end + page.free_bytes();
        if gap_end > capacity {
            return Err(corrupt("free gap does not fit"));
        }

        page.check_fence_slots()?;

        let prefix_len = page.prefix_len();
        let mut payloads = Vec::with_capacity(count);
        let record_start = page.record_start();

        for index in 0..count {
            let slot = page.slots()[index];
            let is_fence = index < record_start;
            let infinity = [LOW_INFINITY, HIGH_INFINITY][index.min(1)];
            if is_fence && slot.offset.get() == infinity {
                continue;
            }

            let omitted = match is_fence && index == 0 {
                true => 0,
                false => prefix_len,
            };
            if slot.key_len() < omitted {
                return Err(corrupt("key shorter than the page prefix"));
            }

            if !is_fence && slot.key_len() == 0 {
                return Err(corrupt("empty record key"));
            }

            let payload = page
                .payload_range(index)
                .expect("finite slots have a payload range");
            if payload.start < gap_end || payload.end > capacity {
                return Err(corrupt("payload outside the payload area"));
            }

            if !is_fence {
                let stored = &bytes[slot.stored_key_range(omitted)];
                let preview_len = stored.len().min(slot.preview.len());
                if slot.preview[..preview_len] != stored[..preview_len]
                    || slot.preview[preview_len..].iter().any(|&byte| byte != 0)
                {
                    return Err(corrupt("slot preview differs from the stored key"));
                }
            }

            payloads.push(payload);
        }

        payloads.sort_unstable_by_key(|payload| payload.start);
        let overlapping = payloads
            .iter()
            .zip(payloads.iter().skip(1))
            .any(|(a, b)| a.end > b.start);
        if overlapping {
            return Err(corrupt("payloads overlap"));
        }

        page.check_prefix_and_fence_order()?;
        page.check_record_order()?;

        Ok(page)
    }

    /// With fences, the first two slots must be the fence pair; each fence is
    /// either its infinity sentinel with no key, or a finite key with no value.
    fn check_fence_slots(&self) -> Result<(), PageError> {
        let corrupt = PageError::Corrupt;
        if self.record_start() == 0 {
            if self.prefix_len() != 0 {
                return Err(corrupt("prefix on an unfenced page"));
            }

            return Ok(());
        }

        if self.slot_count() < 2 {
            return Err(corrupt("fenced page has fewer than two slots"));
        }

        let check = |slot: Slot, infinity: u16| {
            if slot.value_len() != 0 {
                return Err(corrupt("fence slot carries a value"));
            }

            if slot.offset.get() == infinity && slot.key_len() != 0 {
                return Err(corrupt("unbounded fence has a key length"));
            }

            Ok(())
        };

        let lower = self.slots()[0];
        check(lower, LOW_INFINITY)?;
        check(self.slots()[1], HIGH_INFINITY)?;

        // `prefix()` reads the prefix from the low fence's payload, so the
        // prefix must be a finite low fence's leading bytes.
        let prefix_len = self.prefix_len();
        if prefix_len != 0 && (lower.offset.get() == LOW_INFINITY || lower.key_len() < prefix_len) {
            return Err(corrupt("prefix longer than the lower fence"));
        }

        Ok(())
    }

    /// Finite fences must be ordered; a page with an unbounded fence has no
    /// prefix. The high fence decodes as the low fence's prefix plus its own
    /// suffix, so both fences share the prefix by construction.
    fn check_prefix_and_fence_order(&self) -> Result<(), PageError> {
        let corrupt = PageError::Corrupt;
        let Some(fences) = self.fences() else {
            return Ok(());
        };

        match (fences.lower, fences.upper) {
            (Fence::Key(lower), Fence::Key(upper)) if lower >= upper => {
                Err(corrupt("fences are not ordered"))
            }
            (Fence::Key(_), Fence::Key(_)) => Ok(()),
            _ if self.prefix_len() != 0 => Err(corrupt("prefix with an unbounded fence")),
            _ => Ok(()),
        }
    }

    /// Records must be strictly increasing and inside the fences.
    fn check_record_order(&self) -> Result<(), PageError> {
        let corrupt = PageError::Corrupt;
        let sorted = self
            .iter()
            .zip(self.iter().skip(1))
            .all(|(a, b)| a.key < b.key);
        if !sorted {
            return Err(corrupt("record keys are not strictly increasing"));
        }

        // Sorted records make the first and last keys sufficient.
        if let (Some(fences), Some(last)) = (self.fences(), self.len().checked_sub(1)) {
            let first = self.record_at(0).key;
            let last = self.record_at(last).key;
            if let Fence::Key(lower) = fences.lower
                && first < lower
            {
                return Err(corrupt("record below the lower fence"));
            }
            if let Fence::Key(upper) = fences.upper
                && last >= upper
            {
                return Err(corrupt("record at or above the upper fence"));
            }
        }

        Ok(())
    }
}
