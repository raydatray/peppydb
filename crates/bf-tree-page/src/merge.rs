//! Merging a sorted batch of dirty records into a fenced base page.
//!
//! The merged sequence is the page's records with the batch applied: an
//! `Insert` adds or replaces a key, and a `Delete` removes it (a no-op for a
//! key the page does not hold). The output holds only `Insert` records. These
//! functions are the one merge path for mini-page merges and, through a
//! one-record batch, for direct base-page writes.

use zerocopy::IntoBytes;

use super::split::{Merge, unprefixed_size};
use super::{Fences, KeyRef, LeafSplit, Page, PageError, RecordKind, RecordRef};

impl Page {
    /// Whether this fenced page, rebuilt with `batch` applied, fits without a
    /// split. Holes do not count: a rebuild packs the page.
    pub fn merge_batch_fits<'b>(
        &self,
        batch: impl Iterator<Item = RecordRef<'b>> + Clone,
    ) -> Result<bool, PageError> {
        self.check_batch(batch.clone())?;

        Ok(self.merged_size(batch) <= self.record_capacity())
    }

    /// Rebuild this fenced page with `batch` applied. Returns
    /// [`PageError::PageFull`], with the page unchanged, if the result does
    /// not fit. An empty batch leaves the page unchanged.
    pub fn merge_batch_in_place<'b>(
        &mut self,
        batch: impl Iterator<Item = RecordRef<'b>> + Clone,
    ) -> Result<(), PageError> {
        self.check_batch(batch.clone())?;
        if batch.clone().next().is_none() {
            return Ok(());
        }
        if self.merged_size(batch.clone()) > self.record_capacity() {
            return Err(PageError::PageFull);
        }

        let capacity = self.capacity();
        let mut image = [0; 4096];
        {
            let fences = self.fences().expect("check_batch checked the fences");
            let lower = fences.lower.map(KeyRef::to_vec);
            let upper = fences.upper.map(KeyRef::to_vec);
            let fences = Fences {
                lower: lower.as_slice(),
                upper: upper.as_slice(),
            };
            self.build_output(&mut image[..capacity], fences, self.merged(batch));
        }
        self.as_mut_bytes().copy_from_slice(&image[..capacity]);

        Ok(())
    }

    /// Split this fenced page with `batch` applied. `self` becomes the left
    /// output with fences `[lower, separator)` and `right` the right output
    /// with `[separator, upper)`; each output gets the prefix its fences
    /// imply. The separator is the full key of the right output's first
    /// record, each output keeps at least one record, and the split point
    /// balances used bytes. Both outputs inherit this page's header metadata.
    /// Errors, including [`PageError::CannotSplit`], leave both buffers
    /// unchanged.
    pub fn merge_batch_split<'r, 'b>(
        &mut self,
        right: &'r mut [u8],
        batch: impl Iterator<Item = RecordRef<'b>> + Clone,
    ) -> Result<LeafSplit<'r>, PageError> {
        self.check_split_storage(right)?;
        self.check_batch(batch.clone())?;

        let images = self.split_images(self.merged(batch))?;
        Ok(self.commit_split(right, images))
    }

    /// The page's records with `batch` applied, `Insert` records only.
    fn merged<'s, 'b: 's>(
        &'s self,
        batch: impl Iterator<Item = RecordRef<'b>> + Clone,
    ) -> impl Iterator<Item = RecordRef<'s>> + Clone {
        Merge::new(self.iter(), batch.map(RecordRef::shorten))
            .filter(|record| record.state.kind() == RecordKind::Insert)
    }

    /// Record bytes (slots, stored suffixes, values) of the merged page.
    fn merged_size<'b>(&self, batch: impl Iterator<Item = RecordRef<'b>> + Clone) -> usize {
        let prefix = self.prefix_len();
        self.merged(batch)
            .map(|record| unprefixed_size(&record) - prefix)
            .sum()
    }

    /// Check a batch in one pass: the page is fenced, and every record has a
    /// nonempty in-range key, fits an empty page, is dirty, and comes after
    /// the previous one.
    fn check_batch<'b>(&self, batch: impl Iterator<Item = RecordRef<'b>>) -> Result<(), PageError> {
        if self.fences().is_none() {
            return Err(PageError::InvalidFences);
        }

        let mut key = Vec::new();
        let mut previous: Option<Vec<u8>> = None;
        for record in batch {
            key.clear();
            key.extend(record.key.bytes());
            self.record_size(&key, record.state)?;
            if !record.state.kind().is_dirty() {
                return Err(PageError::InvalidBatch("batch holds a clean record"));
            }
            if previous.as_ref().is_some_and(|previous| *previous >= key) {
                return Err(PageError::InvalidBatch(
                    "batch keys are not strictly increasing",
                ));
            }
            previous = Some(key.clone());
        }

        Ok(())
    }
}
