use zerocopy::IntoBytes;

use super::layout::{HAS_FENCES, HEADER_BYTES, HIGH_INFINITY, LOW_INFINITY, SLOT_BYTES, Slot};
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
    /// Checks capacity, finite bound ordering, then fence storage size before
    /// changing any bytes. The returned page borrows only `bytes`.
    pub fn init_with_fences<'b>(
        bytes: &'b mut [u8],
        fences: Fences<&[u8]>,
    ) -> Result<&'b mut Self, PageError> {
        Self::check_capacity(bytes.len())?;
        if let (Fence::Key(lower), Fence::Key(upper)) = (fences.lower, fences.upper)
            && lower >= upper
        {
            return Err(PageError::InvalidFences);
        }

        let needed = fences
            .lower
            .key()
            .map_or(0, <[u8]>::len)
            .checked_add(fences.upper.key().map_or(0, <[u8]>::len))
            .and_then(|size| size.checked_add(2 * SLOT_BYTES))
            .filter(|&size| size <= bytes.len() - HEADER_BYTES)
            .ok_or(PageError::FencesTooLarge)?;

        let page = Self::init(bytes)?;
        page.header.count_and_flags.set(HAS_FENCES | 2);

        let mut end = page.capacity();
        for (index, (fence, infinity)) in
            [(fences.lower, LOW_INFINITY), (fences.upper, HIGH_INFINITY)]
                .into_iter()
                .enumerate()
        {
            let slot = match fence {
                Fence::Unbounded => Slot::new(infinity, b"", 0, RecordKind::Insert),
                Fence::Key(key) => {
                    end -= key.len();
                    page.as_mut_bytes()[end..end + key.len()].copy_from_slice(key);

                    Slot::new(end as u16, key, 0, RecordKind::Insert)
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

        let key = KeyRef::from(key);
        if let Some(fences) = self.fences()
            && (fences.lower.key().is_some_and(|lower| key < lower)
                || fences.upper.key().is_some_and(|upper| key >= upper))
        {
            return Err(PageError::KeyOutOfRange);
        }

        Ok(())
    }

    /// Space available to a single ordinary record, including its slot.
    pub(super) fn record_capacity(&self) -> usize {
        let fence_bytes = self.fences().map_or(0, |fences| {
            2 * SLOT_BYTES
                + fences.lower.key().map_or(0, KeyRef::len)
                + fences.upper.key().map_or(0, KeyRef::len)
        });

        self.capacity() - HEADER_BYTES - fence_bytes
    }
}
