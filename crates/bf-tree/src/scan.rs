use std::iter::FusedIterator;
use std::ops::ControlFlow;

use super::leaf_map::{Locked, Residency, with_scratch};
use super::{Tree, TreeError};
use bf_tree_mini_page::LeafRef;
use bf_tree_page::Fence;

/// One leaf's live records in range, copied, and where the scan continues.
type LeafRecords = (Vec<(Vec<u8>, Vec<u8>)>, Option<Vec<u8>>);

impl Tree {
    /// Iterate over records with keys in `[start, end)` in key order.
    ///
    /// `None` is unbounded; an empty slice is a finite bound. Equal bounds
    /// yield nothing; reversed bounds return [`TreeError::InvalidRange`].
    /// Items are owned copies; [`Tree::scan_with`] is the zero-copy form.
    pub fn range(&self, start: Option<&[u8]>, end: Option<&[u8]>) -> Result<Scan<'_>, TreeError> {
        check_range(start, end)?;

        let cursor = start.unwrap_or_default().to_vec();
        let (records, next) = self.scan_leaf(&cursor, end)?;

        Ok(Scan {
            tree: self,
            records: records.into_iter(),
            next,
            end: end.map(<[u8]>::to_vec),
        })
    }

    /// Iterate over every record in key order.
    pub fn iter(&self) -> Scan<'_> {
        self.range(None, None).expect("unbounded ranges are valid")
    }

    /// Call `visit` with each record in `[start, end)` in key order, until it
    /// returns `Break`. Keys and values are borrowed only for the call. The
    /// visitor runs while the scan reads a leaf, so it must not call back
    /// into the tree.
    pub fn scan_with(
        &self,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
        mut visit: impl FnMut(&[u8], &[u8]) -> ControlFlow<()>,
    ) -> Result<(), TreeError> {
        check_range(start, end)?;

        let n = self.config.leaf_capacity;
        let mut cursor = start.unwrap_or_default().to_vec();
        let mut key = Vec::new();
        loop {
            let leaf = self.read_leaf(&cursor)?;
            let noted = self.note(&leaf);
            let next = with_scratch(|scratch| -> Result<_, TreeError> {
                let page = self.base_of(&leaf, &mut scratch.page[..n])?;
                let view = LeafRef {
                    base: page,
                    mini: mini_of(&leaf),
                };
                for record in view.range(Some(&cursor), end)? {
                    let Some(value) = record.state.value() else {
                        continue;
                    };
                    key.clear();
                    key.extend(record.key.bytes());
                    if visit(&key, value).is_break() {
                        return Ok(None);
                    }
                }
                Ok(next_leaf(
                    page.fences().expect("tree leaves are fenced").upper,
                    end,
                ))
            })?;
            drop(leaf);
            self.maintain(noted)?;
            match next {
                Some(next) => cursor = next,
                None => return Ok(()),
            }
        }
    }

    /// Copy the live records in `[cursor, end)` from the leaf containing
    /// `cursor`, and that leaf's finite upper fence if the scan continues
    /// past it. The lower bound applies to every leaf, not only the first.
    fn scan_leaf(&self, cursor: &[u8], end: Option<&[u8]>) -> Result<LeafRecords, TreeError> {
        let n = self.config.leaf_capacity;
        let leaf = self.read_leaf(cursor)?;
        let noted = self.note(&leaf);
        let result = with_scratch(|scratch| {
            let page = self.base_of(&leaf, &mut scratch.page[..n])?;
            let view = LeafRef {
                base: page,
                mini: mini_of(&leaf),
            };
            let records = view
                .range(Some(cursor), end)?
                .filter_map(|record| {
                    let value = record.state.value()?;
                    Some((record.key.to_vec(), value.to_vec()))
                })
                .collect();
            let next = next_leaf(page.fences().expect("tree leaves are fenced").upper, end);

            Ok((records, next))
        });
        drop(leaf);
        self.maintain(noted)?;
        result
    }
}

impl Tree {
    /// Lock the leaf that owns `key` shared: route, lock, validate, retry.
    pub(super) fn read_leaf(&self, key: &[u8]) -> Result<super::leaf_map::LeafRead<'_>, TreeError> {
        loop {
            let route = self.route(key);
            let leaf = self.leaves.read(route.leaf)?;
            if !self.route_changed(&route) {
                return Ok(leaf);
            }
        }
    }
}

impl Tree {
    /// The leaf's base page: its full page if it has one, else the store's
    /// copy loaded into `buf`.
    fn base_of<'p>(
        &self,
        leaf: &'p impl Locked,
        buf: &'p mut [u8],
    ) -> Result<&'p bf_tree_page::Page, TreeError> {
        match leaf.residency() {
            Residency::Full(page) => Ok(page),
            _ => Ok(self.leaves.load_base(leaf, buf)?),
        }
    }
}

/// The leaf's mini-page, if it has one.
fn mini_of(leaf: &impl Locked) -> Option<&bf_tree_mini_page::MiniPage> {
    match leaf.residency() {
        Residency::Mini { page, .. } => Some(page),
        Residency::Full(_) | Residency::Base => None,
    }
}

fn check_range(start: Option<&[u8]>, end: Option<&[u8]>) -> Result<(), TreeError> {
    if let (Some(start), Some(end)) = (start, end)
        && start > end
    {
        return Err(TreeError::InvalidRange);
    }
    Ok(())
}

/// Where a scan continues after a leaf with upper fence `upper`: its key if
/// it is finite and below `end`.
fn next_leaf(upper: Fence<bf_tree_page::KeyRef<'_>>, end: Option<&[u8]>) -> Option<Vec<u8>> {
    match upper {
        Fence::Key(upper) if end.is_none_or(|end| upper.cmp_bytes(end).is_lt()) => {
            Some(upper.to_vec())
        }
        _ => None,
    }
}

/// Records of a [`Tree`] in key order, as owned key-value pairs.
///
/// Created by [`Tree::range`] or [`Tree::iter`]. The scan copies one leaf's
/// records at a time. Moving to the next leaf may load it from the page
/// store, so items are `Result`s; after an error the scan ends.
#[derive(Clone, Debug)]
pub struct Scan<'t> {
    tree: &'t Tree,
    records: std::vec::IntoIter<(Vec<u8>, Vec<u8>)>,
    /// The current leaf's finite upper fence, if the scan continues past it.
    next: Option<Vec<u8>>,
    end: Option<Vec<u8>>,
}

impl Iterator for Scan<'_> {
    type Item = Result<(Vec<u8>, Vec<u8>), TreeError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(record) = self.records.next() {
                return Some(Ok(record));
            }

            let next = self.next.take()?;
            match self.tree.scan_leaf(&next, self.end.as_deref()) {
                Ok((records, next)) => (self.records, self.next) = (records.into_iter(), next),
                Err(error) => return Some(Err(error)),
            }
        }
    }
}

impl FusedIterator for Scan<'_> {}
