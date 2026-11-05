use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use std::ops::{Deref, DerefMut};

use bf_tree_circular_buffer::Block;
use bf_tree_node_id::LeafId;
use bf_tree_page::{Page, PageError, RecordRef, RecordState, UpsertOutcome};

use super::RefBits;

/// An unfenced page whose `backing_leaf` names the leaf it overlays.
#[derive(Debug, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct MiniPage(Page);

/// What [`MiniPage::apply`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Apply {
    Applied,
    /// A clean record arrived for a key with a dirty record, which is newer
    /// than the base page; the dirty record stays.
    SkippedDirty,
}

/// The answer a mini-page gives for a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup<'a> {
    /// A live value: `Insert` or `Cache`.
    Found(&'a [u8]),
    /// A known absence: `Delete` or `Phantom`.
    Absent,
    /// Not buffered; the base page decides.
    Miss,
}

impl MiniPage {
    /// Initialize an empty mini-page for `backing` over `bytes`. Panics unless
    /// `bytes.len()` is a legal page capacity, as every size class is.
    pub fn init(bytes: &mut [u8], backing: LeafId) -> &mut Self {
        let page = Page::init(bytes).expect("size classes are legal capacities");
        page.set_backing_leaf(Some(backing));
        Self::from_bytes_mut(bytes)
    }

    /// View bytes that hold a mini-page. Panics if they are too short for a
    /// page header.
    pub fn from_bytes(bytes: &[u8]) -> &Self {
        Self::ref_from_bytes(bytes).expect("mini-page bytes hold a page header")
    }

    pub fn from_bytes_mut(bytes: &mut [u8]) -> &mut Self {
        Self::mut_from_bytes(bytes).expect("mini-page bytes hold a page header")
    }

    pub fn page(&self) -> &Page {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn page_mut(&mut self) -> &mut Page {
        &mut self.0
    }

    /// The leaf this mini-page overlays.
    pub fn backing_leaf(&self) -> LeafId {
        self.0
            .backing_leaf()
            .expect("mini-pages name their backing leaf")
    }

    /// Look up `key` with one search and mark its reference bit on a hit.
    /// An empty key is a `Miss`.
    pub fn lookup_marking(&self, refs: &RefBits, key: &[u8]) -> Lookup<'_> {
        let Ok(Some(index)) = self.0.find_index(key) else {
            return Lookup::Miss;
        };
        refs.mark(index);
        match self.0.record_at(index).state {
            RecordState::Insert(value) | RecordState::Cache(value) => Lookup::Found(value),
            RecordState::Delete | RecordState::Phantom => Lookup::Absent,
        }
    }
}

impl MiniPage {
    /// Insert or replace a record, keeping reference bit `i` with slot `i`.
    ///
    /// | Incoming | Nothing buffered | Dirty buffered | Clean buffered |
    /// | --- | --- | --- | --- |
    /// | `Insert(v)` | Insert | Replace | Replace |
    /// | `Delete` | Key-only tombstone | Replace | Replace |
    /// | `Cache(v)`, `Phantom` | Insert | `SkippedDirty` | Replace |
    ///
    /// A `Delete` of an unbuffered key leaves a tombstone, because the base
    /// page may hold the key. Returns `PageFull` or `RecordTooLarge` with the
    /// page unchanged; it never compacts or grows.
    pub fn apply(
        &mut self,
        refs: &mut RefBits,
        key: &[u8],
        state: RecordState<'_>,
    ) -> Result<Apply, PageError> {
        if let Some(index) = self.0.find_index(key)?
            && self.0.record_at(index).state.kind().is_dirty()
            && !state.kind().is_dirty()
        {
            return Ok(Apply::SkippedDirty);
        }

        let (outcome, index) = self.0.upsert_indexed(key, state)?;
        if outcome == UpsertOutcome::Inserted {
            refs.insert(index);
        }
        Ok(Apply::Applied)
    }

    /// Pack the payloads; slot positions, and so reference bits, are kept.
    pub fn compact(&mut self) -> usize {
        self.0.compact()
    }

    /// Rebuild into `dst` with the same backing leaf. With
    /// `discard_cold_clean`, `Cache` and `Phantom` records whose bit is clear
    /// are dropped. The caller starts the copy with clear bits. Panics if the
    /// kept records do not fit `dst`; callers copy into a class at least as
    /// large as the content.
    pub fn copy_into<'d>(
        &self,
        refs: &RefBits,
        dst: &'d mut [u8],
        discard_cold_clean: bool,
    ) -> &'d mut Self {
        let copy = MiniPage::init(dst, self.backing_leaf());
        let mut key = Vec::new();
        for (index, record) in self.0.iter().enumerate() {
            if discard_cold_clean && !record.state.kind().is_dirty() && !refs.is_set(index) {
                continue;
            }
            key.clear();
            key.extend(record.key.bytes());
            copy.0
                .try_insert_record(&key, record.state)
                .expect("the destination holds the kept records");
        }
        copy
    }

    /// `Insert` and `Delete` records in key order: the batch a merge applies.
    pub fn dirty(&self) -> impl Iterator<Item = RecordRef<'_>> + Clone {
        self.0
            .iter()
            .filter(|record| record.state.kind().is_dirty())
    }
}

/// One mini-page's storage: its circular-buffer block and the reference
/// bits beside it.
#[derive(Debug)]
pub struct Mini {
    pub block: Block,
    pub refs: Box<RefBits>,
}

impl Mini {
    /// Initialize `block` as an empty mini-page for `backing`.
    pub fn new(mut block: Block, backing: LeafId) -> Self {
        MiniPage::init(block.as_mut(), backing);
        Self {
            block,
            refs: Box::default(),
        }
    }

    pub fn lookup_marking(&self, key: &[u8]) -> Lookup<'_> {
        MiniPage::from_bytes(self.block.as_ref()).lookup_marking(&self.refs, key)
    }

    pub fn apply(&mut self, key: &[u8], state: RecordState<'_>) -> Result<Apply, PageError> {
        MiniPage::from_bytes_mut(self.block.as_mut()).apply(&mut self.refs, key, state)
    }

    /// Copy into `dst`, as [`MiniPage::copy_into`], and clear its bits.
    pub fn copy_into(&self, dst: &mut Mini, discard_cold_clean: bool) {
        MiniPage::from_bytes(self.block.as_ref()).copy_into(
            &self.refs,
            dst.block.as_mut(),
            discard_cold_clean,
        );
        dst.refs.clear();
    }
}

impl Deref for Mini {
    type Target = MiniPage;

    fn deref(&self) -> &MiniPage {
        MiniPage::from_bytes(self.block.as_ref())
    }
}

impl DerefMut for Mini {
    fn deref_mut(&mut self) -> &mut MiniPage {
        MiniPage::from_bytes_mut(self.block.as_mut())
    }
}
