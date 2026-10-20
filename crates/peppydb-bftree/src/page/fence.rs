use zerocopy::IntoBytes;

use super::layout::{
    HAS_FENCES, HEADER_BYTES, HIGH_INFINITY, LOW_INFINITY, MAX_KEY_LEN, SLOT_BYTES, Slot,
};
use super::{KeyRef, Page, PageError, RecordKind};

/// A page bound. A finite empty key is distinct from an unbounded endpoint.
///
/// [`Page::init_with_fences`] accepts full keys as `Fence<&[u8]>`;
/// [`Page::fences`] returns logical views as `Fence<KeyRef>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fence<K> {
    /// Negative infinity for a lower fence; positive infinity for an upper fence.
    Unbounded,
    Key(K),
}

/// Lower-inclusive, upper-exclusive page bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fences<K> {
    pub lower: Fence<K>,
    pub upper: Fence<K>,
}

impl<K> Fence<K> {
    fn key(self) -> Option<K> {
        match self {
            Self::Unbounded => None,
            Self::Key(key) => Some(key),
        }
    }
}

impl Page {
    /// Initialize an empty fenced page, copying finite bounds into its storage.
    ///
    /// With two finite bounds, the page stores their longest common prefix once,
    /// as the start of the full low fence, and omits it from the high fence and
    /// every ordinary record. Every key in `[lower, upper)` shares that prefix.
    /// Unbounded or one-sided pages use an empty prefix.
    ///
    /// Checks capacity, finite bound ordering, then compressed fence storage
    /// size before changing any bytes. The returned page borrows only `bytes`.
    pub fn init_with_fences<'b>(
        bytes: &'b mut [u8],
        fences: Fences<&[u8]>,
    ) -> Result<&'b mut Self, PageError> {
        Self::check_capacity(bytes.len())?;
        let prefix_len = match (fences.lower, fences.upper) {
            (Fence::Key(lower), Fence::Key(upper)) if lower >= upper => {
                return Err(PageError::InvalidFences);
            }
            (Fence::Key(lower), Fence::Key(upper)) => lower
                .iter()
                .zip(upper)
                .take_while(|(lower, upper)| lower == upper)
                .count(),
            _ => 0,
        };
        let entries = [
            (fences.lower, LOW_INFINITY, 0),
            (fences.upper, HIGH_INFINITY, prefix_len),
        ];
        let stored = |(fence, _, omitted): (Fence<&[u8]>, u16, usize)| {
            fence.key().map_or(Some(0), |key| {
                (key.len() <= MAX_KEY_LEN).then(|| key.len() - omitted)
            })
        };

        let needed = entries
            .into_iter()
            .map(stored)
            .try_fold(2 * SLOT_BYTES, |size, stored| size.checked_add(stored?))
            .filter(|&size| size <= bytes.len() - HEADER_BYTES)
            .ok_or(PageError::FencesTooLarge)?;

        let page = Self::init(bytes)?;
        page.header.count_and_flags.set(HAS_FENCES | 2);
        // The fit check bounds the prefix by the capacity.
        page.header.prefix_len.set(prefix_len as u16);

        let mut end = page.capacity();
        for (index, (fence, infinity, omitted)) in entries.into_iter().enumerate() {
            let slot = match fence {
                Fence::Unbounded => Slot::new(infinity, b"", 0, 0, RecordKind::Insert),
                Fence::Key(key) => {
                    let suffix = &key[omitted..];
                    end -= suffix.len();
                    page.as_mut_bytes()[end..end + suffix.len()].copy_from_slice(suffix);

                    Slot::new(end as u16, key, omitted, 0, RecordKind::Insert)
                }
            };
            page.slots_mut()[index] = slot;
        }

        page.header
            .remaining_size
            .set((page.capacity() - HEADER_BYTES - needed) as u16);

        Ok(page)
    }

    /// Borrow the page's structural bounds, or `None` on an unfenced page.
    pub fn fences(&self) -> Option<Fences<KeyRef<'_>>> {
        if self.record_start() == 0 {
            return None;
        }

        // Check for infinity sentinels before slicing.
        let prefix = self.prefix();
        let decode = |index: usize, infinity, omitted| {
            let prefix = &prefix[..omitted];
            let slot = self.slots()[index];

            if slot.offset.get() == infinity {
                return Fence::Unbounded;
            }

            let suffix = slot.stored_key_range(prefix.len());
            Fence::Key(KeyRef::new(prefix, &self.as_bytes()[suffix]))
        };

        Some(Fences {
            lower: decode(0, LOW_INFINITY, 0),
            upper: decode(1, HIGH_INFINITY, prefix.len()),
        })
    }

    /// Reject empty and out-of-range keys. Out-of-range keys are errors, not misses.
    pub(super) fn check_key(&self, key: &[u8]) -> Result<(), PageError> {
        if key.is_empty() {
            return Err(PageError::EmptyKey);
        }

        if let Some(fences) = self.fences()
            && (fences
                .lower
                .key()
                .is_some_and(|lower| lower.cmp_bytes(key).is_gt())
                || fences
                    .upper
                    .key()
                    .is_some_and(|upper| upper.cmp_bytes(key).is_le()))
        {
            return Err(PageError::KeyOutOfRange);
        }

        Ok(())
    }

    /// Space available to a single ordinary record, including its slot.
    pub(super) fn record_capacity(&self) -> usize {
        let fence_bytes: usize = (0..self.record_start())
            .map(|index| SLOT_BYTES + self.payload_range(index).map_or(0, |payload| payload.len()))
            .sum();

        self.capacity() - HEADER_BYTES - fence_bytes
    }
}
