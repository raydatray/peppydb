use std::iter::{Flatten, FusedIterator};
use std::option;

use bf_tree_page::{Iter, LeafSplit, Merge, Page, PageError, RecordRef};

use super::MiniPage;

/// A logical leaf for reading: its base page and its mini-page, if any.
#[derive(Clone, Copy, Debug)]
pub struct LeafRef<'a> {
    pub base: &'a Page,
    pub mini: Option<&'a MiniPage>,
}

impl<'a> LeafRef<'a> {
    /// Live records in `[start, end)`: base and mini-page merged, the
    /// mini-page winning an equal key, and `Delete` or `Phantom` hiding their
    /// keys. Bounds follow [`Page::range`]. A scan always reads the base page,
    /// because a mini-page may hold only part of the leaf.
    pub fn range(self, start: Option<&[u8]>, end: Option<&[u8]>) -> Result<Scan<'a>, PageError> {
        let base = self.base.range(start, end)?;
        let mini = self
            .mini
            .map(|mini| mini.page().range(start, end))
            .transpose()?;
        Ok(Scan(Merge::new(base, mini.into_iter().flatten())))
    }
}

/// Live records of a [`LeafRef`] in key order. Created by [`LeafRef::range`].
#[derive(Clone, Debug)]
pub struct Scan<'a>(Merge<Iter<'a>, Flatten<option::IntoIter<Iter<'a>>>>);

impl<'a> Iterator for Scan<'a> {
    type Item = RecordRef<'a>;

    fn next(&mut self) -> Option<RecordRef<'a>> {
        self.0
            .by_ref()
            .find(|record| record.state.value().is_some())
    }
}

impl FusedIterator for Scan<'_> {}

/// A logical leaf for merging: the base page to rewrite and the mini-page
/// whose dirty records it absorbs. The merge borrows the mini-page and never
/// frees it; the caller frees the block afterwards.
#[derive(Debug)]
pub struct LeafMut<'a> {
    pub base: &'a mut Page,
    pub mini: Option<&'a MiniPage>,
}

impl<'a> LeafMut<'a> {
    /// Whether the base page absorbs the mini-page's dirty records without a
    /// split.
    pub fn merge_fits(&self) -> Result<bool, PageError> {
        self.base.merge_batch_fits(dirty(self.mini))
    }

    /// Rewrite the base page with the mini-page's dirty records applied.
    /// `PageFull`, with the base unchanged, if the result does not fit.
    pub fn merge_in_place(&mut self) -> Result<(), PageError> {
        self.base.merge_batch_in_place(dirty(self.mini))
    }

    /// Split the merged leaf: the base page becomes the left half and `right`
    /// the right half. Errors leave both buffers unchanged.
    pub fn merge_split<'r>(&mut self, right: &'r mut [u8]) -> Result<LeafSplit<'r>, PageError> {
        self.base.merge_batch_split(right, dirty(self.mini))
    }
}

/// The dirty records of `mini`, or none.
fn dirty(mini: Option<&MiniPage>) -> impl Iterator<Item = RecordRef<'_>> + Clone {
    mini.into_iter().flat_map(MiniPage::dirty)
}
