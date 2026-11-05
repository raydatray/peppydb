use std::cell::RefCell;

use super::{TreeConfig, TreeError};
use bf_tree_circular_buffer::{Block, CircularBuffer};
use bf_tree_mini_page::{Mini, MiniPage, RefBits, SizeClasses, WriteOutcome};
use bf_tree_node_id::LeafId;
use bf_tree_page::{Page, PageError};
use bf_tree_store::PageStore;
use bf_tree_sync::{RwLock, RwLockReadGuard, RwLockWriteGuard, Table};

/// The leaf mapping table: one reader-writer lock per stable [`LeafId`].
///
/// The lock is the paper's per-leaf reader-writer lock (§3.3). It covers the
/// entry and the leaf's base page in the store: every base write happens
/// under the exclusive lock, so a base page read under the shared lock is
/// consistent. Base pages are not resident: an access loads one into a
/// thread-local scratch buffer. A leaf's mini-page, if any, lives in the
/// circular buffer and is reached through its entry.
#[derive(Debug)]
pub(super) struct LeafMap {
    store: Box<dyn PageStore>,
    entries: Table<RwLock<LeafEntry>>,
    /// Every mini-page block. Its mutex is innermost: it may be taken under
    /// a leaf lock, and it never waits for one.
    pub(super) circular_buffer: CircularBuffer,
    pub(super) classes: SizeClasses,
}

#[derive(Debug, Default)]
pub(super) struct LeafEntry {
    /// The slot holds no page.
    freed: bool,
    /// A circular-buffer block holding this leaf's mini-page.
    block: Option<Block>,
    /// Reference bits for the mini-page's slots; `Some` exactly when `block`
    /// is.
    refs: Option<Box<RefBits>>,
    // Later plans add `version` (0014) and `dirty` (0015). The size test
    // guards the entry.
}

/// What a leaf holds in memory besides its base page in the store.
pub(super) enum Residency<'e> {
    Base,
    Mini {
        page: &'e MiniPage,
        refs: &'e RefBits,
    },
}

impl LeafEntry {
    fn residency(&self) -> Residency<'_> {
        match (&self.block, &self.refs) {
            (Some(block), Some(refs)) => Residency::Mini {
                page: MiniPage::from_bytes(block.as_ref()),
                refs,
            },
            _ => Residency::Base,
        }
    }
}

/// A leaf held under its shared lock.
pub(super) struct LeafRead<'m> {
    id: LeafId,
    entry: RwLockReadGuard<'m, LeafEntry>,
}

/// A leaf held under its exclusive lock.
pub(super) struct LeafWrite<'m> {
    id: LeafId,
    entry: RwLockWriteGuard<'m, LeafEntry>,
}

/// A held leaf lock, shared or exclusive. Base pages are read only through
/// one.
pub(super) trait Locked {
    fn id(&self) -> LeafId;
    fn residency(&self) -> Residency<'_>;
}

impl Locked for LeafRead<'_> {
    fn id(&self) -> LeafId {
        self.id
    }

    fn residency(&self) -> Residency<'_> {
        self.entry.residency()
    }
}

impl Locked for LeafWrite<'_> {
    fn id(&self) -> LeafId {
        self.id
    }

    fn residency(&self) -> Residency<'_> {
        self.entry.residency()
    }
}

impl LeafWrite<'_> {
    /// Move the mini-page's two handles out of the entry; no bytes move.
    pub(super) fn take_mini(&mut self) -> Option<Mini> {
        match (self.entry.block.take(), self.entry.refs.take()) {
            (Some(block), Some(refs)) => Some(Mini { block, refs }),
            (None, None) => None,
            _ => unreachable!("refs are present exactly with a block"),
        }
    }

    /// Move a mini-page's handles back into the entry.
    pub(super) fn put_mini(&mut self, mini: Option<Mini>) {
        debug_assert!(self.entry.block.is_none());
        if let Some(Mini { block, refs }) = mini {
            self.entry.block = Some(block);
            self.entry.refs = Some(refs);
        }
    }
}

impl LeafMap {
    /// Map the store's existing slots without loading any of them.
    /// `freed` marks the slots that hold no page.
    pub(super) fn new(
        store: Box<dyn PageStore>,
        freed: impl IntoIterator<Item = bool>,
        circular_buffer: CircularBuffer,
        classes: SizeClasses,
    ) -> Self {
        let entries: Table<RwLock<LeafEntry>> = Table::new();
        for (index, freed) in freed.into_iter().enumerate() {
            entries.grow_to(index + 1);
            entries.get(index).expect("just grown").write().freed = freed;
        }

        Self {
            store,
            entries,
            circular_buffer,
            classes,
        }
    }

    /// A map over `store` with a circular buffer and ladder from a validated
    /// `config`.
    pub(super) fn for_config(
        store: Box<dyn PageStore>,
        freed: impl IntoIterator<Item = bool>,
        config: &TreeConfig,
    ) -> Result<Self, TreeError> {
        let classes = config.size_classes()?;
        let circular_buffer = CircularBuffer::new(
            config.circular_buffer_bytes,
            &config.buffer_classes(&classes),
            0,
        )
        .map_err(|_| TreeError::InvalidConfig("invalid circular-buffer budget"))?;
        Ok(Self::new(store, freed, circular_buffer, classes))
    }

    /// Number of slots, live or freed; IDs are `0..len()`.
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn store(&self) -> &dyn PageStore {
        self.store.as_ref()
    }

    /// Give up the map and return its store. Buffered mini-pages are lost;
    /// flush first to keep them. Their blocks return to the circular buffer
    /// before it is dropped.
    pub(super) fn into_store(self) -> Box<dyn PageStore> {
        let Self {
            store,
            entries,
            circular_buffer,
            ..
        } = self;
        for entry in entries.iter() {
            if let Some(block) = entry.write().block.take() {
                circular_buffer.free(block);
            }
        }
        store
    }

    /// The live leaves' IDs as of now; a by-ID walk such as `flush` uses
    /// them.
    pub(super) fn ids(&self) -> impl Iterator<Item = LeafId> + use<> {
        (0..self.entries.len()).map(LeafId::from_index)
    }

    /// Buffer one record in `leaf`'s mini-page, creating or growing it. This
    /// is the only place that allocates mini-page blocks. Never reads the
    /// base page.
    pub(super) fn buffer(
        &self,
        leaf: &mut LeafWrite<'_>,
        key: &[u8],
        state: bf_tree_page::RecordState<'_>,
    ) -> Result<WriteOutcome, TreeError> {
        let mut slot = leaf.take_mini();
        let result = bf_tree_mini_page::buffer(
            &mut slot,
            &self.circular_buffer,
            leaf.id,
            &self.classes,
            key,
            state,
        );
        leaf.put_mini(slot);
        Ok(result?)
    }

    /// Lock leaf `id` exclusively if it is a live slot.
    pub(super) fn write_if_live(&self, id: LeafId) -> Option<LeafWrite<'_>> {
        self.write(id).ok()
    }

    /// Number of leaves holding a mini-page. Takes each shared lock in turn.
    pub(super) fn buffered(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.read().block.is_some())
            .count()
    }

    fn entry(&self, id: LeafId) -> Result<&RwLock<LeafEntry>, TreeError> {
        self.entries
            .get(id.index())
            .ok_or(TreeError::UnknownLeaf(id))
    }

    /// Whether `id` is an allocated slot that holds a page. Takes the entry's
    /// shared lock briefly.
    #[cfg(test)]
    pub(super) fn is_live(&self, id: LeafId) -> bool {
        self.entry(id).is_ok_and(|entry| !entry.read().freed)
    }

    /// Lock leaf `id` shared. `UnknownLeaf` if it is not a live slot.
    pub(super) fn read(&self, id: LeafId) -> Result<LeafRead<'_>, TreeError> {
        let entry = self.entry(id)?.read();
        if entry.freed {
            return Err(TreeError::UnknownLeaf(id));
        }
        Ok(LeafRead { id, entry })
    }

    /// Lock leaf `id` exclusively. `UnknownLeaf` if it is not a live slot.
    pub(super) fn write(&self, id: LeafId) -> Result<LeafWrite<'_>, TreeError> {
        let entry = self.entry(id)?.write();
        if entry.freed {
            return Err(TreeError::UnknownLeaf(id));
        }
        Ok(LeafWrite { id, entry })
    }

    /// Allocate a fresh slot and return it locked exclusively. It is
    /// unreachable until a separator names it.
    pub(super) fn alloc(&self) -> Result<LeafWrite<'_>, TreeError> {
        let id = self.store.alloc()?;
        self.entries.grow_to(id.index() + 1);
        let mut entry = self.entry(id)?.write();
        *entry = LeafEntry::default();
        Ok(LeafWrite { id, entry })
    }

    /// Return a fresh, never-reachable slot to the store, for a split that
    /// failed after `alloc`.
    pub(super) fn discard(&self, mut leaf: LeafWrite<'_>) -> Result<(), TreeError> {
        // Free the slot first: if that fails, the entry stays live and the
        // slot keeps what was written, rather than claiming a free slot.
        self.store.free(leaf.id)?;
        leaf.entry.freed = true;
        Ok(())
    }

    /// Read leaf's base page into `buf` and check its header. This is the
    /// only function that reads a base page; the guard proves the caller
    /// holds the leaf's lock.
    ///
    /// `open` validated every page fully. Pages written since were produced
    /// by this process, so loads check only the O(1) header, and debug builds
    /// validate fully so that tests exercise it.
    pub(super) fn load_base<'s>(
        &self,
        leaf: &impl Locked,
        buf: &'s mut [u8],
    ) -> Result<&'s mut Page, TreeError> {
        let id = leaf.id();
        self.store.read(id, buf)?;
        let corrupt = |error| match error {
            PageError::Corrupt(reason) => TreeError::CorruptLeaf { id, reason },
            other => TreeError::Page(other),
        };
        if cfg!(debug_assertions) {
            Page::validate(buf).map_err(corrupt)?;
        }
        Page::check_header(buf, buf.len()).map_err(corrupt)?;

        Ok(zerocopy::FromBytes::mut_from_bytes(buf).expect("checked above"))
    }

    /// Write base pages to the store. The caller must hold the exclusive lock
    /// of every leaf in `pages` and pass the reachable leaf first. The others
    /// are written before it, so an error before the last write leaves the
    /// reachable leaf's stored page unchanged. Takes no tree-level lock.
    pub(super) fn write_through(&self, pages: &mut [(LeafId, &mut [u8])]) -> Result<(), TreeError> {
        let (first, rest) = pages.split_first_mut().expect("at least one page");
        for (id, bytes) in rest.iter_mut().chain([first]) {
            debug_assert!(
                self.entry(*id)?.try_read().is_none(),
                "the caller holds the leaf's exclusive lock"
            );
            self.store.write(*id, bytes)?;
        }
        Ok(())
    }

    /// Make every completed write durable.
    pub(super) fn sync(&self) -> Result<(), TreeError> {
        self.store.sync()?;
        Ok(())
    }
}

#[cfg(test)]
impl LeafMap {
    /// Apply `records` to leaf `id`'s mini-page, as they are, without growing
    /// it or merging. A leaf without a mini-page gets one, allocated from the
    /// circular buffer at the smallest class that holds the records. Test
    /// fixtures build buffered leaves, including clean records, this way.
    /// `PageFull` if an existing mini-page cannot take the records; records
    /// applied before the failure stay.
    pub(super) fn install_mini(
        &self,
        id: LeafId,
        records: &[(&[u8], bf_tree_page::RecordState<'_>)],
    ) -> Result<(), TreeError> {
        let mut leaf = self.write(id)?;
        let mut mini = match leaf.take_mini() {
            Some(mini) => mini,
            None => {
                let bytes: usize = records
                    .iter()
                    .map(|(key, state)| 8 + key.len() + state.value().map_or(0, <[u8]>::len))
                    .sum();
                let class = self
                    .classes
                    .smallest_for(bytes)
                    .ok_or(TreeError::Page(PageError::RecordTooLarge))?;
                let block = self
                    .circular_buffer
                    .alloc(class, bf_tree_circular_buffer::Kind::Mini, id)
                    .map_err(|_| TreeError::CircularBufferFull)?;
                Mini::new(block, id)
            }
        };
        let applied = records
            .iter()
            .try_for_each(|(key, state)| mini.apply(key, *state).map(drop));
        leaf.put_mini(Some(mini));
        Ok(applied?)
    }

    /// Point leaf `id`'s mini-page at another leaf, to test the checker.
    pub(super) fn corrupt_mini_backing(&self, id: LeafId, other: LeafId) {
        let mut leaf = self.write(id).unwrap();
        let mut mini = leaf.take_mini().expect("a mini-page");
        let page: &mut Page = zerocopy::FromBytes::mut_from_bytes(mini.block.as_mut()).unwrap();
        page.set_backing_leaf(Some(other));
        leaf.put_mini(Some(mini));
    }

    /// A copy of leaf `id`'s mini-page bytes, if it has one.
    pub(super) fn mini_copy(&self, id: LeafId) -> Option<Vec<u8>> {
        let leaf = self.read(id).ok()?;
        match leaf.residency() {
            Residency::Mini { page, .. } => Some(zerocopy::IntoBytes::as_bytes(page).to_vec()),
            Residency::Base => None,
        }
    }

    /// A copy of leaf `id`'s base page, read under its shared lock.
    pub(super) fn page_copy(&self, id: LeafId) -> Result<Vec<u8>, TreeError> {
        let leaf = self.read(id)?;
        let mut bytes = vec![0; self.store.page_size()];
        self.load_base(&leaf, &mut bytes)?;
        Ok(bytes)
    }

    /// Overwrite leaf `id`'s base page under its exclusive lock, without
    /// checking the bytes. Test fixtures build pages this way.
    pub(super) fn overwrite(&self, id: LeafId, bytes: &mut [u8]) -> Result<(), TreeError> {
        let leaf = self.write(id)?;
        self.write_through(&mut [(leaf.id(), bytes)])
    }

    /// Allocate a slot and return its ID, unlocked and unreachable.
    pub(super) fn alloc_id(&self) -> Result<LeafId, TreeError> {
        Ok(self.alloc()?.id())
    }
}

/// The largest legal page; scratch buffers are sliced to the tree's size.
pub(super) const MAX_PAGE: usize = 4096;

/// One thread's buffers for loading base pages.
pub(super) struct Scratch {
    pub(super) page: Box<[u8; MAX_PAGE]>,
    /// The right half of a split.
    pub(super) right: Box<[u8; MAX_PAGE]>,
}

impl Scratch {
    fn new() -> Self {
        Self {
            page: Box::new([0; MAX_PAGE]),
            right: Box::new([0; MAX_PAGE]),
        }
    }
}

thread_local! {
    static SCRATCH: RefCell<Option<Scratch>> = const { RefCell::new(None) };
}

/// Lend this thread's scratch to `f`. Tree code does not nest calls; a
/// nested call from a user callback gets a temporary scratch instead of a
/// panic, and the outer scratch goes back to the thread afterwards.
pub(super) fn with_scratch<R>(f: impl FnOnce(&mut Scratch) -> R) -> R {
    let mut scratch = SCRATCH
        .with(|cell| cell.borrow_mut().take())
        .unwrap_or_else(Scratch::new);
    let result = f(&mut scratch);
    SCRATCH.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(scratch);
        }
    });
    result
}
