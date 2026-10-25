use std::iter::FusedIterator;

use super::{Tree, TreeError};
use crate::page::Iter;
use crate::{Fence, RecordRef};

impl Tree {
    /// Iterate over records with keys in `[start, end)` in key order.
    ///
    /// `None` is unbounded; an empty slice is a finite bound. Equal bounds
    /// yield nothing; reversed bounds return [`TreeError::InvalidRange`].
    pub fn range(&self, start: Option<&[u8]>, end: Option<&[u8]>) -> Result<Scan<'_>, TreeError> {
        if let (Some(start), Some(end)) = (start, end)
            && start > end
        {
            return Err(TreeError::InvalidRange);
        }

        let (records, next) = self.scan_leaf(start.unwrap_or_default(), start, end)?;

        Ok(Scan {
            tree: self,
            records,
            next,
            end: end.map(<[u8]>::to_vec),
        })
    }

    /// Iterate over every record in key order.
    pub fn iter(&self) -> Scan<'_> {
        self.range(None, None).expect("unbounded ranges are valid")
    }

    /// Borrow the records in `[start, end)` from the leaf containing `key`,
    /// and that leaf's finite upper fence if the scan continues past it.
    fn scan_leaf(
        &self,
        key: &[u8],
        start: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Result<(Iter<'_>, Option<Vec<u8>>), TreeError> {
        let page = self.leaf(self.find_leaf(key))?;
        let records = page.range(start, end)?;
        let next = match page.fences().expect("tree leaves are fenced").upper {
            Fence::Key(upper) if end.is_none_or(|end| upper.cmp_bytes(end).is_lt()) => {
                Some(upper.to_vec())
            }
            _ => None,
        };

        Ok((records, next))
    }
}

/// Records of a [`Tree`] in key order, borrowed from its leaves.
///
/// Created by [`Tree::range`] or [`Tree::iter`]. Moving to the next leaf may
/// load it from the page store, so items are `Result`s; after an error the
/// scan ends.
#[derive(Clone, Debug)]
pub struct Scan<'t> {
    tree: &'t Tree,
    records: Iter<'t>,
    /// The current leaf's finite upper fence, if the scan continues past it.
    next: Option<Vec<u8>>,
    end: Option<Vec<u8>>,
}

impl<'t> Iterator for Scan<'t> {
    type Item = Result<RecordRef<'t>, TreeError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(record) = self.records.next() {
                return Some(Ok(record));
            }

            let next = self.next.take()?;
            match self.tree.scan_leaf(&next, None, self.end.as_deref()) {
                Ok((records, next)) => (self.records, self.next) = (records, next),
                Err(error) => return Some(Err(error)),
            }
        }
    }
}

impl FusedIterator for Scan<'_> {}
