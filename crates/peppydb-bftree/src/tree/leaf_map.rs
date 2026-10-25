use std::cell::{Cell, OnceCell};

use zerocopy::FromBytes;

use super::TreeError;
use crate::store::PageStore;
use crate::{LeafId, Page, PageError};

/// The leaf mapping table: resolves a stable [`LeafId`] to the leaf's
/// current bytes, loading them from the [`PageStore`] on first access.
///
/// Entries are dense and indexed by the ID. A resident copy, once loaded, is
/// never dropped in this stack, so shared borrows of a page stay valid for as
/// long as the map does. Eviction is a later milestone and will need `&mut`
/// access or a latch.
pub(super) struct LeafMap {
    store: Box<dyn PageStore>,
    entries: Vec<LeafEntry>,
}

#[derive(Default)]
struct LeafEntry {
    /// Full-page copy, loaded on first access.
    resident: OnceCell<Box<[u8]>>,
    /// The resident copy is newer than the store.
    dirty: Cell<bool>,
    /// The slot is on the store's free list; never true in this stack's tree.
    freed: bool,
}

impl LeafEntry {
    fn resident_mut(&mut self) -> &mut [u8] {
        self.resident
            .get_mut()
            .expect("the leaf was made resident before mutation")
    }
}

impl std::fmt::Debug for LeafMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeafMap")
            .field("slots", &self.store.slots())
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl LeafMap {
    /// Map the store's existing slots without loading any of them.
    /// `freed` marks the slots that hold no page.
    pub(super) fn new(store: Box<dyn PageStore>, freed: impl IntoIterator<Item = bool>) -> Self {
        let entries = freed
            .into_iter()
            .map(|freed| LeafEntry {
                freed,
                ..LeafEntry::default()
            })
            .collect();

        Self { store, entries }
    }

    /// Number of slots, live or freed; IDs are `0..len()`.
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn store(&self) -> &dyn PageStore {
        self.store.as_ref()
    }

    /// Give up the map and return its store. Unflushed changes are lost.
    pub(super) fn into_store(self) -> Box<dyn PageStore> {
        self.store
    }

    /// Whether `id` is an allocated slot that holds a page. A slot can be
    /// allocated but freed after a reopen of a store whose last allocations
    /// were never flushed.
    pub(super) fn is_live(&self, id: LeafId) -> bool {
        self.entries
            .get(id.index())
            .is_some_and(|entry| !entry.freed)
    }

    pub(super) fn resident_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.resident.get().is_some())
            .count()
    }

    pub(super) fn dirty_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.dirty.get())
            .count()
    }

    /// Allocate a slot whose resident copy is zeroed and dirty. The caller
    /// initializes the page before the next flush.
    pub(super) fn alloc(&mut self) -> Result<LeafId, TreeError> {
        let id = self.store.alloc()?;
        let entry = LeafEntry {
            resident: OnceCell::from(vec![0; self.store.page_size()].into_boxed_slice()),
            dirty: Cell::new(true),
            freed: false,
        };

        let index = id.index();
        if index == self.entries.len() {
            self.entries.push(entry);
        } else {
            self.entries[index] = entry;
        }

        Ok(id)
    }

    /// Borrow a leaf page, loading and validating it on first access.
    pub(super) fn page(&self, id: LeafId) -> Result<&Page, TreeError> {
        let bytes = self.bytes(id)?;

        Ok(Page::ref_from_bytes(bytes).expect("resident leaves hold a page header"))
    }

    /// Borrow a leaf page for mutation and mark it dirty.
    pub(super) fn page_mut(&mut self, id: LeafId) -> Result<&mut Page, TreeError> {
        let bytes = self.bytes_mut(id)?;

        Ok(Page::mut_from_bytes(bytes).expect("resident leaves hold a page header"))
    }

    /// Borrow a leaf's raw bytes for mutation and mark it dirty. Used to
    /// initialize a freshly allocated leaf.
    pub(super) fn bytes_mut(&mut self, id: LeafId) -> Result<&mut [u8], TreeError> {
        self.bytes(id)?;
        let entry = &mut self.entries[id.index()];
        entry.dirty.set(true);

        Ok(entry.resident_mut())
    }

    /// Borrow two distinct leaves mutably at once and mark both dirty.
    pub(super) fn pair_mut(
        &mut self,
        a: LeafId,
        b: LeafId,
    ) -> Result<(&mut [u8], &mut [u8]), TreeError> {
        self.bytes(a)?;
        self.bytes(b)?;
        let [a, b] = self
            .entries
            .get_disjoint_mut([a.index(), b.index()])
            .expect("distinct allocated IDs");
        a.dirty.set(true);
        b.dirty.set(true);

        Ok((a.resident_mut(), b.resident_mut()))
    }

    /// Write every dirty leaf to the store, then sync. A failed write leaves
    /// that leaf and every later dirty leaf still dirty.
    pub(super) fn flush(&mut self) -> Result<(), TreeError> {
        for (index, entry) in self.entries.iter().enumerate() {
            if !entry.dirty.get() {
                continue;
            }

            let bytes = entry.resident.get().expect("dirty leaves are resident");
            self.store.write(LeafId::from_index(index), bytes)?;
            entry.dirty.set(false);
        }
        self.store.sync()?;

        Ok(())
    }

    /// The resident bytes of `id`, loaded through `&self` on first access.
    fn bytes(&self, id: LeafId) -> Result<&[u8], TreeError> {
        if !self.is_live(id) {
            return Err(TreeError::UnknownLeaf(id));
        }
        let entry = &self.entries[id.index()];

        if entry.resident.get().is_none() {
            let mut bytes = vec![0; self.store.page_size()].into_boxed_slice();
            self.store.read(id, &mut bytes)?;
            Page::validate(&bytes).map_err(|error| match error {
                PageError::Corrupt(reason) => TreeError::CorruptLeaf { id, reason },
                other => TreeError::Page(other),
            })?;
            // Only this call initializes the cell, and nothing clears it.
            let _ = entry.resident.set(bytes);
        }

        Ok(entry.resident.get().expect("set above"))
    }
}
