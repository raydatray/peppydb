use zerocopy::IntoBytes;

use super::layout::{HAS_FENCES, HEADER_BYTES, HIGH_INFINITY, LOW_INFINITY, SLOT_BYTES, Slot};
use super::{Page, PageError, RecordKind};

/// A page bound. A finite empty key is distinct from an unbounded endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fence<'a> {
    /// Negative infinity for a lower fence; positive infinity for an upper fence.
    Unbounded,
    Key(&'a [u8]),
}

/// Lower-inclusive, upper-exclusive page bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fences<'a> {
    pub lower: Fence<'a>,
    pub upper: Fence<'a>,
}

impl<'a> Fence<'a> {
    fn key(self) -> Option<&'a [u8]> {
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
        fences: Fences<'_>,
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
    pub fn fences(&self) -> Option<Fences<'_>> {
        if self.record_start() == 0 {
            return None;
        }

        let decode = |index: usize, infinity| {
            let slot = self.slots()[index];

            if slot.offset.get() == infinity {
                return Fence::Unbounded;
            }

            Fence::Key(&self.as_bytes()[slot.key_range()])
        };

        Some(Fences {
            lower: decode(0, LOW_INFINITY),
            upper: decode(1, HIGH_INFINITY),
        })
    }

    /// Point operations validate routing before distinguishing a hit from a miss.
    pub(super) fn check_key(&self, key: &[u8]) -> Result<(), PageError> {
        if key.is_empty() {
            return Err(PageError::EmptyKey);
        }

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
                + fences.lower.key().map_or(0, <[u8]>::len)
                + fences.upper.key().map_or(0, <[u8]>::len)
        });

        self.capacity() - HEADER_BYTES - fence_bytes
    }
}
