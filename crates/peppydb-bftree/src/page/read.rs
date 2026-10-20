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

        let index = self.lower_bound(key);
        if index == self.len() {
            return Ok(None);
        }

        let record = self.record_at(index);
        Ok((record.key == KeyRef::from(key)).then_some(record))
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

        let start = start.map_or(0, |key| self.lower_bound(key));
        let end = end.map_or(self.len(), |key| self.lower_bound(key));

        Ok(Iter {
            page: self,
            indices: start..end,
        })
    }

    pub(super) fn key_at(&self, index: usize) -> &[u8] {
        &self.as_bytes()[self.record_slots()[index].key_range()]
    }

    pub(super) fn record_at(&self, index: usize) -> RecordRef<'_> {
        let slot = self.record_slots()[index];
        let key = slot.key_range();
        let value = key.end..key.end + slot.value_len();
        let bytes = self.as_bytes();

        let state = match slot.kind() {
            RecordKind::Insert => RecordState::Insert(&bytes[value]),
            RecordKind::Delete => RecordState::Delete,
            RecordKind::Cache => RecordState::Cache(&bytes[value]),
            RecordKind::Phantom => RecordState::Phantom,
        };

        RecordRef {
            key: KeyRef::from(&bytes[key]),
            state,
            referenced: slot.referenced(),
        }
    }

    pub(super) fn lower_bound(&self, key: &[u8]) -> usize {
        self.record_slots()
            .partition_point(|slot| self.compare_key(slot, key).is_lt())
    }

    /// Compare previews first; load the stored key only on a tie.
    pub(super) fn compare_key(&self, slot: &Slot, key: &[u8]) -> Ordering {
        let stored_preview = &slot.preview[..slot.key_range().len().min(PREVIEW_BYTES)];
        let query_preview = &key[..key.len().min(PREVIEW_BYTES)];

        stored_preview.cmp(query_preview).then_with(|| {
            let stored_key = &self.as_bytes()[slot.key_range()];
            stored_key.cmp(key)
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
