use std::iter::{FusedIterator, Peekable};
use std::slice;

use zerocopy::{FromBytes, IntoBytes};

use super::layout::{HEADER_BYTES, SLOT_BYTES};
use super::{Fence, Fences, Iter, KeyRef, Page, PageError, RecordRef, RecordState};

/// The result of [`Page::split_with_insert`].
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
    /// Split a fenced page together with one pending record.
    ///
    /// `self` becomes the left output with fences `[lower, separator)` and
    /// `right_storage` the right output with `[separator, upper)`; each output
    /// gets the prefix its fences imply. The separator is the full key of the
    /// right output's first record, each output keeps at least one record, and
    /// the split point balances used bytes.
    ///
    /// Record kinds and reference bits are preserved; tombstones lose their
    /// retained value bytes, as in any rebuild. Both outputs inherit this page's
    /// backing-leaf, LSN, and snapshot-version fields. Errors, including
    /// [`PageError::CannotSplit`], leave both buffers unchanged.
    pub fn split_with_insert<'r>(
        &mut self,
        right_storage: &'r mut [u8],
        key: &[u8],
        state: RecordState<'_>,
    ) -> Result<LeafSplit<'r>, PageError> {
        self.check_split_storage(right_storage)?;
        self.record_size(key, state)?;
        if self.search(key).is_ok() {
            return Err(PageError::DuplicateKey);
        }

        let pending = [RecordRef {
            key: KeyRef::from(key),
            state,
            referenced: false,
        }];

        self.split_with_batch(right_storage, &pending)
    }

    fn check_split_storage(&self, right_storage: &[u8]) -> Result<(), PageError> {
        Self::check_capacity(right_storage.len())?;

        if right_storage.len() != self.capacity() {
            return Err(PageError::CapacityMismatch);
        }

        if self.fences().is_none() {
            return Err(PageError::InvalidFences);
        }

        Ok(())
    }

    /// Split the virtual merge of this page's records and a sorted batch of
    /// in-range pending records. A pending record replaces a stored record with
    /// the same key. The caller has validated the storage and every record.
    fn split_with_batch<'r>(
        &mut self,
        right_storage: &'r mut [u8],
        pending: &[RecordRef<'_>],
    ) -> Result<LeafSplit<'r>, PageError> {
        let capacity = self.capacity();
        let fences = self.fences().expect("caller checked the fences");
        let records = Merge::new(self.iter(), pending);
        let index = choose_split(records.clone(), fences, self.prefix_len(), capacity)
            .ok_or(PageError::CannotSplit)?;

        // `records` borrows from `self`, which becomes the left output, so build
        // both sides in scratch images first.
        let separator = records
            .clone()
            .nth(index)
            .expect("index < count")
            .key
            .to_vec();

        let lower = fences.lower.map(KeyRef::to_vec);
        let upper = fences.upper.map(KeyRef::to_vec);
        let mut left_image = [0; 4096];
        let mut right_image = [0; 4096];
        let left_fences = Fences {
            lower: lower.as_slice(),
            upper: Fence::Key(separator.as_slice()),
        };
        let right_fences = Fences {
            lower: Fence::Key(separator.as_slice()),
            upper: upper.as_slice(),
        };
        let left = &mut left_image[..capacity];
        let right = &mut right_image[..capacity];
        self.build_output(left, left_fences, records.clone().take(index));
        self.build_output(right, right_fences, records.skip(index));

        self.as_mut_bytes().copy_from_slice(left);
        right_storage.copy_from_slice(right);
        let right = Page::mut_from_bytes(right_storage).expect("capacity holds the header");

        Ok(LeafSplit { separator, right })
    }

    fn build_output<'a>(
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

            if record.referenced {
                page.set_referenced(&key, true)
                    .expect("the record was just inserted");
            }
        }

        page.header.backing_leaf.set(self.header.backing_leaf.get());
        page.header.lsn.set(self.header.lsn.get());
        page.header
            .snapshot_version
            .set(self.header.snapshot_version.get());
    }
}

impl<K> Fence<K> {
    fn map<T>(self, f: impl FnOnce(K) -> T) -> Fence<T> {
        match self {
            Self::Unbounded => Fence::Unbounded,
            Self::Key(key) => Fence::Key(f(key)),
        }
    }
}

impl Fence<Vec<u8>> {
    fn as_slice(&self) -> Fence<&[u8]> {
        match self {
            Self::Unbounded => Fence::Unbounded,
            Self::Key(key) => Fence::Key(key),
        }
    }
}

/// Stored records merged in key order with a sorted pending batch. A pending
/// record replaces a stored record with the same key.
#[derive(Clone, Debug)]
pub(super) struct Merge<'a> {
    stored: Peekable<Iter<'a>>,
    pending: Peekable<slice::Iter<'a, RecordRef<'a>>>,
}

impl<'a> Merge<'a> {
    pub(super) fn new(stored: Iter<'a>, pending: &'a [RecordRef<'a>]) -> Self {
        Self {
            stored: stored.peekable(),
            pending: pending.iter().peekable(),
        }
    }
}

impl<'a> Iterator for Merge<'a> {
    type Item = RecordRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let Some(pending) = self.pending.peek() else {
            return self.stored.next();
        };

        match self.stored.peek() {
            Some(stored) if stored.key < pending.key => self.stored.next(),
            Some(stored) => {
                if stored.key == pending.key {
                    self.stored.next();
                }

                self.pending.next().copied()
            }
            None => self.pending.next().copied(),
        }
    }
}

impl FusedIterator for Merge<'_> {}

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
