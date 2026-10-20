use std::iter::FusedIterator;
use std::ops::Range;

use zerocopy::IntoBytes;

use super::{Page, PageError};

impl Page {
    /// Borrow the value for a nonempty key, or return `None` if it is absent.
    ///
    /// An empty value is present and returns `Some(&[])`.
    pub fn get(&self, key: &[u8]) -> Result<Option<&[u8]>, PageError> {
        if key.is_empty() {
            return Err(PageError::EmptyKey);
        }

        let index = self.lower_bound(key);
        if index == self.len() {
            return Ok(None);
        }

        let (stored_key, value) = self.record_at(index);
        Ok((stored_key == key).then_some(value))
    }

    /// Borrow key/value pairs in lexicographic key order without allocation.
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            page: self,
            indices: 0..self.len(),
        }
    }

    /// Borrow the key addressed by slot `index` without decoding its value.
    pub(super) fn key_at(&self, index: usize) -> &[u8] {
        &self.as_bytes()[self.slots()[index].key_range()]
    }

    pub(super) fn record_at(&self, index: usize) -> (&[u8], &[u8]) {
        let slot = self.slots()[index];
        let key = slot.key_range();
        let value = key.end..key.end + slot.value_len();
        let bytes = self.as_bytes();

        (&bytes[key], &bytes[value])
    }

    /// Index of the first slot whose key is not less than `key`.
    pub(super) fn lower_bound(&self, key: &[u8]) -> usize {
        let bytes = self.as_bytes();

        self.slots()
            .partition_point(|slot| &bytes[slot.key_range()] < key)
    }
}

impl<'p> IntoIterator for &'p Page {
    type Item = (&'p [u8], &'p [u8]);
    type IntoIter = Iter<'p>;

    fn into_iter(self) -> Iter<'p> {
        self.iter()
    }
}

/// Borrowed key/value pairs of a [`Page`] in lexicographic key order.
///
/// Created by [`Page::iter`] or by iterating over `&Page`.
#[derive(Clone, Debug)]
pub struct Iter<'p> {
    page: &'p Page,
    indices: Range<usize>,
}

impl<'p> Iterator for Iter<'p> {
    type Item = (&'p [u8], &'p [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        self.indices.next().map(|index| self.page.record_at(index))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.indices.size_hint()
    }
}

impl ExactSizeIterator for Iter<'_> {}

impl FusedIterator for Iter<'_> {}
