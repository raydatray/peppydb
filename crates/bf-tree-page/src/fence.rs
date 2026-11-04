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

        let prefix_len = Self::fence_prefix_len(fences)?;

        // The low fence stores its full key; the high fence omits the prefix.
        let lower = fences.lower.key().unwrap_or_default();
        let upper = fences.upper.key().unwrap_or_default();
        if lower.len() > MAX_KEY_LEN || upper.len() > MAX_KEY_LEN {
            return Err(PageError::FencesTooLarge);
        }

        let lower_len = lower.len();
        let upper_len = upper.len() - prefix_len;
        let needed = 2 * SLOT_BYTES + lower_len + upper_len;
        if needed > bytes.len() - HEADER_BYTES {
            return Err(PageError::FencesTooLarge);
        }

        // Validation is complete; from here on, the buffer changes.
        let page = Self::init(bytes)?;
        page.header.count_and_flags.set(HAS_FENCES | 2);
        // The fit check bounds the prefix by the capacity.
        page.header.prefix_len.set(prefix_len as u16);
        page.header
            .remaining_size
            .set((page.capacity() - HEADER_BYTES - needed) as u16);

        // Fence payloads sit at the end of the page: low fence last, high just below.
        let lower_offset = page.capacity() - lower_len;
        let upper_offset = lower_offset - upper_len;
        page.install_fence(0, fences.lower, lower_offset);
        page.install_fence(1, fences.upper, upper_offset);

        Ok(page)
    }

    /// The longest common prefix of two finite fences, or 0 if either is unbounded.
    fn fence_prefix_len(fences: Fences<&[u8]>) -> Result<usize, PageError> {
        let (Fence::Key(lower), Fence::Key(upper)) = (fences.lower, fences.upper) else {
            return Ok(0);
        };

        if lower >= upper {
            return Err(PageError::InvalidFences);
        }

        Ok(lower.iter().zip(upper).take_while(|(a, b)| a == b).count())
    }

    /// Store fence `index` (0 = low, 1 = high): copy its stored bytes to `offset`
    /// and write its slot. Requires the header's fence flag, count, and prefix.
    fn install_fence(&mut self, index: usize, fence: Fence<&[u8]>, offset: usize) {
        let (sentinel, omitted) = match index {
            0 => (LOW_INFINITY, 0),
            _ => (HIGH_INFINITY, self.prefix_len()),
        };

        let slot = match fence {
            Fence::Unbounded => Slot::new(sentinel, b"", 0, 0, RecordKind::Insert),
            Fence::Key(key) => {
                let stored = &key[omitted..];
                self.as_mut_bytes()[offset..offset + stored.len()].copy_from_slice(stored);

                Slot::new(offset as u16, key, omitted, 0, RecordKind::Insert)
            }
        };

        self.slots_mut()[index] = slot;
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
        let fence_bytes: usize = (0..self.record_start())
            .map(|index| SLOT_BYTES + self.payload_range(index).map_or(0, |payload| payload.len()))
            .sum();

        self.capacity() - HEADER_BYTES - fence_bytes
    }
}
