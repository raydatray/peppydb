use std::cmp::Ordering;

/// A borrowed logical key stored as a leading prefix slice followed by a suffix.
///
/// The parts may borrow disjoint regions: a compressed page's shared prefix and
/// a record's suffix, or an inner separator's inline bytes and its tail.
/// Equality and ordering use the logical concatenated bytes, so the split point
/// never affects a comparison: `"ab" + "c"` equals `"a" + "bc"`. Accessors
/// return slices with the borrowed lifetime `'p`, not the lifetime of the view
/// value.
///
/// ```
/// use peppydb_bftree::KeyRef;
///
/// let key = KeyRef::from(b"acct/123".as_slice());
/// assert_eq!(key.len(), 8);
/// assert!(key.cmp_bytes(b"acct/2").is_lt());
/// assert_eq!(key.to_vec(), b"acct/123");
/// ```
#[derive(Clone, Copy, Debug)]
pub struct KeyRef<'p> {
    prefix: &'p [u8],
    suffix: &'p [u8],
}

impl<'p> KeyRef<'p> {
    pub(crate) fn new(prefix: &'p [u8], suffix: &'p [u8]) -> Self {
        Self { prefix, suffix }
    }

    pub fn prefix(self) -> &'p [u8] {
        self.prefix
    }

    pub fn suffix(self) -> &'p [u8] {
        self.suffix
    }

    pub fn len(self) -> usize {
        self.prefix.len() + self.suffix.len()
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    pub fn bytes(self) -> impl DoubleEndedIterator<Item = u8> + Clone + 'p {
        self.prefix
            .iter()
            .copied()
            .chain(self.suffix.iter().copied())
    }

    /// Compare the logical key with a contiguous key lexicographically.
    pub fn cmp_bytes(self, other: &[u8]) -> Ordering {
        let (head, tail) = other.split_at(self.prefix.len().min(other.len()));
        // A prefix longer than `other` compares greater on a tie, as required.
        self.prefix.cmp(head).then_with(|| self.suffix.cmp(tail))
    }

    pub fn to_vec(self) -> Vec<u8> {
        [self.prefix, self.suffix].concat()
    }
}

impl<'p> From<&'p [u8]> for KeyRef<'p> {
    /// View a contiguous key with an empty prefix.
    fn from(key: &'p [u8]) -> Self {
        Self::new(b"", key)
    }
}

impl PartialEq for KeyRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.cmp(other).is_eq()
    }
}

impl Eq for KeyRef<'_> {}

impl PartialOrd for KeyRef<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for KeyRef<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        if other.prefix.is_empty() {
            self.cmp_bytes(other.suffix)
        } else if self.prefix.is_empty() {
            other.cmp_bytes(self.suffix).reverse()
        } else {
            self.bytes().cmp(other.bytes())
        }
    }
}
