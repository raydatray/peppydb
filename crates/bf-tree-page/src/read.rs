use std::cmp::Ordering;
use std::iter::FusedIterator;
use std::ops::Range;

use zerocopy::IntoBytes;

use super::layout::{PREVIEW_BYTES, Slot};
use super::{KeyRef, Page, PageError, RecordKind, RecordRef, RecordState};

impl Page {
    /// Borrow a stored record for a nonempty key, or `None` on a page miss.
    ///
    /// Tombstones and phantoms return `Some` with their explicit absence state.
    /// Reads leave all bytes, including reference bits, unchanged.
    pub fn get(&self, key: &[u8]) -> Result<Option<RecordRef<'_>>, PageError> {
        self.check_key(key)?;

        Ok(self.search(key).ok().map(|index| self.record_at(index)))
    }

    /// Borrow all record states in lexicographic key order without allocation.
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            page: self,
            indices: 0..self.len(),
        }
    }

    /// Borrow records in `[start, end)` using two lower-bound searches.
    ///
    /// `None` is unbounded; an empty byte slice is a finite bound. Equal bounds
    /// yield an empty iterator; reversed bounds return [`PageError::InvalidRange`].
    /// Bounds may extend beyond page fences. The result borrows only the page,
    /// includes every record state, and takes `O(log n + output)` comparisons.
    pub fn range(&self, start: Option<&[u8]>, end: Option<&[u8]>) -> Result<Iter<'_>, PageError> {
        if let (Some(start), Some(end)) = (start, end)
            && start > end
        {
            return Err(PageError::InvalidRange);
        }

        let bound = |key: &[u8]| self.search(key).unwrap_or_else(|index| index);
        let start = start.map_or(0, bound);
        let end = end.map_or(self.len(), bound);

        Ok(Iter {
            page: self,
            indices: start..end,
        })
    }

    pub(super) fn record_at(&self, index: usize) -> RecordRef<'_> {
        let slot = self.record_slots()[index];
        let suffix = self.suffix_range(&slot);
        let value = suffix.end..suffix.end + slot.value_len();
        let bytes = self.as_bytes();

        let state = match slot.kind() {
            RecordKind::Insert => RecordState::Insert(&bytes[value]),
            RecordKind::Delete => RecordState::Delete,
            RecordKind::Cache => RecordState::Cache(&bytes[value]),
            RecordKind::Phantom => RecordState::Phantom,
        };

        RecordRef {
            key: KeyRef::new(self.prefix(), &bytes[suffix]),
            state,
            referenced: slot.referenced(),
        }
    }

    /// `Ok(index)` if `key` is stored, else `Err(insertion index)`.
    ///
    /// Every record key starts with the prefix, so strip it once, then
    /// binary-search the stored suffixes. A key without the prefix sorts
    /// before or after every record.
    pub(super) fn search(&self, key: &[u8]) -> Result<usize, usize> {
        let prefix = self.prefix();
        match key.strip_prefix(prefix) {
            Some(query) => self
                .record_slots()
                .binary_search_by(|slot| self.compare_suffix(slot, query)),
            None if key < prefix => Err(0),
            None => Err(self.len()),
        }
    }

    /// Compare previews first; load the stored suffix only on a tie.
    pub(super) fn compare_suffix(&self, slot: &Slot, query: &[u8]) -> Ordering {
        let stored_len = slot.key_len() - self.prefix_len();
        let stored_preview = &slot.preview[..stored_len.min(PREVIEW_BYTES)];
        let query_preview = &query[..query.len().min(PREVIEW_BYTES)];

        stored_preview.cmp(query_preview).then_with(|| {
            let stored = &self.as_bytes()[self.suffix_range(slot)];
            stored.cmp(query)
        })
    }
}

impl<'p> IntoIterator for &'p Page {
    type Item = RecordRef<'p>;
    type IntoIter = Iter<'p>;

    fn into_iter(self) -> Iter<'p> {
        self.iter()
    }
}

/// Borrowed records of a [`Page`] in lexicographic key order, including absence.
///
/// Created by [`Page::iter`], [`Page::range`], or by iterating over `&Page`.
#[derive(Clone, Debug)]
pub struct Iter<'p> {
    page: &'p Page,
    indices: Range<usize>,
}

impl<'p> Iterator for Iter<'p> {
    type Item = RecordRef<'p>;

    fn next(&mut self) -> Option<Self::Item> {
        self.indices.next().map(|index| self.page.record_at(index))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.indices.size_hint()
    }
}

impl ExactSizeIterator for Iter<'_> {}

impl FusedIterator for Iter<'_> {}
