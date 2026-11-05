use std::fmt::Debug;
use std::iter::{FusedIterator, Peekable};

use zerocopy::{FromBytes, IntoBytes};

use super::layout::{HEADER_BYTES, SLOT_BYTES};
use super::{Fence, Fences, KeyRef, Page, PageError, RecordRef};

/// The result of [`Page::merge_batch_split`].
#[derive(Debug)]
pub struct LeafSplit<'r> {
    /// The full key of the right output's first record. It is the right
    /// output's lower fence, the left output's upper fence, and the separator
    /// that the parent routes on.
    pub separator: Vec<u8>,
    /// The right output, initialized in the caller's storage.
    pub right: &'r mut Page,
}

impl Page {
    pub(super) fn check_split_storage(&self, right_storage: &[u8]) -> Result<(), PageError> {
        Self::check_capacity(right_storage.len())?;

        if right_storage.len() != self.capacity() {
            return Err(PageError::CapacityMismatch);
        }

        if self.fences().is_none() {
            return Err(PageError::InvalidFences);
        }

        Ok(())
    }

    /// Build both outputs of a split of `records`, a sorted sequence that may
    /// borrow this page, into scratch images. The page is unchanged.
    pub(super) fn split_images<'a>(
        &self,
        records: impl Iterator<Item = RecordRef<'a>> + Clone,
    ) -> Result<SplitImages, PageError> {
        let capacity = self.capacity();
        let fences = self.fences().expect("caller checked the fences");
        let index = choose_split(records.clone(), fences, self.prefix_len(), capacity)
            .ok_or(PageError::CannotSplit)?;

        let separator = records
            .clone()
            .nth(index)
            .expect("index < count")
            .key
            .to_vec();

        let lower = fences.lower.map(KeyRef::to_vec);
        let upper = fences.upper.map(KeyRef::to_vec);
        let mut images = SplitImages {
            left: [0; 4096],
            right: [0; 4096],
            separator,
        };
        let left_fences = Fences {
            lower: lower.as_slice(),
            upper: Fence::Key(images.separator.as_slice()),
        };
        let right_fences = Fences {
            lower: Fence::Key(images.separator.as_slice()),
            upper: upper.as_slice(),
        };
        self.build_output(
            &mut images.left[..capacity],
            left_fences,
            records.clone().take(index),
        );
        self.build_output(
            &mut images.right[..capacity],
            right_fences,
            records.skip(index),
        );

        Ok(images)
    }

    /// Copy split outputs into `self` (left) and `right_storage` (right).
    pub(super) fn commit_split<'r>(
        &mut self,
        right_storage: &'r mut [u8],
        images: SplitImages,
    ) -> LeafSplit<'r> {
        let capacity = self.capacity();
        self.as_mut_bytes()
            .copy_from_slice(&images.left[..capacity]);
        right_storage.copy_from_slice(&images.right[..capacity]);
        let right = Page::mut_from_bytes(right_storage).expect("capacity holds the header");

        LeafSplit {
            separator: images.separator,
            right,
        }
    }

    pub(super) fn build_output<'a>(
        &self,
        image: &mut [u8],
        fences: Fences<&[u8]>,
        records: impl Iterator<Item = RecordRef<'a>>,
    ) {
        let page = Page::init_with_fences(image, fences).expect("selection budgeted the fences");
        let mut key = Vec::new();

        for record in records {
            key.clear();
            key.extend(record.key.bytes());
            page.try_insert_record(&key, record.state)
                .expect("selection budgeted every record");
        }

        page.header.backing_leaf.set(self.header.backing_leaf.get());
        page.header.lsn.set(self.header.lsn.get());
        page.header
            .snapshot_version
            .set(self.header.snapshot_version.get());
    }
}

/// Both outputs of a split, built before either page changes.
pub(super) struct SplitImages {
    left: [u8; 4096],
    right: [u8; 4096],
    separator: Vec<u8>,
}

impl<K> Fence<K> {
    pub(super) fn map<T>(self, f: impl FnOnce(K) -> T) -> Fence<T> {
        match self {
            Self::Unbounded => Fence::Unbounded,
            Self::Key(key) => Fence::Key(f(key)),
        }
    }
}

impl Fence<Vec<u8>> {
    pub(super) fn as_slice(&self) -> Fence<&[u8]> {
        match self {
            Self::Unbounded => Fence::Unbounded,
            Self::Key(key) => Fence::Key(key),
        }
    }
}

/// Two sorted record sequences in key order. On an equal key the record from
/// `newer` is yielded and the one from `older` is skipped.
///
/// Both sides must yield the same `RecordRef` lifetime; a longer-lived side
/// converts with `.map(RecordRef::shorten)`.
pub struct Merge<A: Iterator, B: Iterator> {
    older: Peekable<A>,
    newer: Peekable<B>,
}

impl<A: Iterator + Clone, B: Iterator + Clone> Clone for Merge<A, B>
where
    A::Item: Clone,
    B::Item: Clone,
{
    fn clone(&self) -> Self {
        Self {
            older: self.older.clone(),
            newer: self.newer.clone(),
        }
    }
}

impl<A: Iterator + Debug, B: Iterator + Debug> Debug for Merge<A, B>
where
    A::Item: Debug,
    B::Item: Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Merge")
            .field("older", &self.older)
            .field("newer", &self.newer)
            .finish()
    }
}

impl<A: Iterator, B: Iterator> Merge<A, B> {
    pub fn new(older: A, newer: B) -> Self {
        Self {
            older: older.peekable(),
            newer: newer.peekable(),
        }
    }
}

impl<'a, A, B> Iterator for Merge<A, B>
where
    A: Iterator<Item = RecordRef<'a>>,
    B: Iterator<Item = RecordRef<'a>>,
{
    type Item = RecordRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let Some(newer) = self.newer.peek() else {
            return self.older.next();
        };

        match self.older.peek() {
            Some(older) if older.key < newer.key => self.older.next(),
            Some(older) => {
                if older.key == newer.key {
                    self.older.next();
                }

                self.newer.next()
            }
            None => self.newer.next(),
        }
    }
}

impl<'a, A, B> FusedIterator for Merge<A, B>
where
    A: FusedIterator<Item = RecordRef<'a>>,
    B: FusedIterator<Item = RecordRef<'a>>,
{
}

/// Bytes a record occupies with an empty page prefix: its slot, full key, and
/// logical value. Absence states store no value.
pub(super) fn unprefixed_size(record: &RecordRef<'_>) -> usize {
    SLOT_BYTES + record.key.len() + record.state.value().map_or(0, <[u8]>::len)
}

/// Fence slot and payload bytes: the lower fence stores its full key, the upper
/// fence its suffix after `prefix`, and an unbounded fence no payload.
fn fence_bytes(lower: Fence<KeyRef<'_>>, upper: Fence<KeyRef<'_>>, prefix: usize) -> usize {
    let lower = match lower {
        Fence::Unbounded => 0,
        Fence::Key(key) => key.len(),
    };
    let upper = match upper {
        Fence::Unbounded => 0,
        Fence::Key(key) => key.len() - prefix,
    };

    2 * SLOT_BYTES + lower + upper
}

/// The prefix that fences `[lower, upper)` imply: their longest common prefix,
/// or zero if either is unbounded. Both keys already share `known` bytes.
fn implied_prefix(lower: Fence<KeyRef<'_>>, upper: Fence<KeyRef<'_>>, known: usize) -> usize {
    let (Fence::Key(lower), Fence::Key(upper)) = (lower, upper) else {
        return 0;
    };

    let shared = lower
        .bytes()
        .skip(known)
        .zip(upper.bytes().skip(known))
        .take_while(|(a, b)| a == b)
        .count();

    known + shared
}

/// Choose the index of the first right-output record, or `None`.
///
/// Splitting at `j` keeps records `0..j` on the left with fences
/// `[lower, key_j)` and `j..` on the right with `[key_j, upper)`. Both
/// endpoints are excluded so each output keeps a record. Candidates whose
/// outputs exceed `capacity` are discarded; the rest are scored by
/// `(max used, used difference, j)`. Every key and finite fence shares the
/// source page's `known` prefix bytes, so each prefix comparison starts there.
pub(super) fn choose_split<'a>(
    records: impl Iterator<Item = RecordRef<'a>> + Clone,
    fences: Fences<KeyRef<'_>>,
    known: usize,
    capacity: usize,
) -> Option<usize> {
    let (count, total) = records.clone().fold((0, 0), |(count, total), record| {
        (count + 1, total + unprefixed_size(&record))
    });

    records
        .enumerate()
        .scan(0, |before, (index, record)| {
            let left_raw = *before;
            *before += unprefixed_size(&record);

            Some((index, record.key, left_raw))
        })
        .filter(|&(index, ..)| index > 0)
        .map(|(index, key, left_raw)| {
            let key = Fence::Key(key);
            let left_prefix = implied_prefix(fences.lower, key, known);
            let right_prefix = implied_prefix(key, fences.upper, known);
            let left = HEADER_BYTES + fence_bytes(fences.lower, key, left_prefix) + left_raw
                - left_prefix * index;
            let right = HEADER_BYTES + fence_bytes(key, fences.upper, right_prefix) + total
                - left_raw
                - right_prefix * (count - index);

            (index, left, right)
        })
        .filter(|&(_, left, right)| left <= capacity && right <= capacity)
        .min_by_key(|&(index, left, right)| (left.max(right), left.abs_diff(right), index))
        .map(|(index, ..)| index)
}
