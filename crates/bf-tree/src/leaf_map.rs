use std::cell::RefCell;

use super::TreeError;
use bf_tree_node_id::LeafId;
use bf_tree_page::{Page, PageError};
use bf_tree_store::PageStore;
use bf_tree_sync::{RwLock, RwLockReadGuard, RwLockWriteGuard, Table};

/// The leaf mapping table: one reader-writer lock per stable [`LeafId`].
///
/// The lock is the paper's per-leaf reader-writer lock (§3.3). It covers the
/// entry and the leaf's base page in the store: every base write happens
/// under the exclusive lock, so a base page read under the shared lock is
/// consistent. No page stays resident; each access loads the base page into
/// a thread-local scratch buffer.
#[derive(Debug)]
pub(super) struct LeafMap {
    store: Box<dyn PageStore>,
    entries: Table<RwLock<LeafEntry>>,
}

#[derive(Debug, Default)]
pub(super) struct LeafEntry {
    /// The slot holds no page.
    freed: bool,
    // Later plans add fields: `block` and `refs` (0013), `version` (0014),
    // `dirty` (0015). The size test guards them.
}

/// A leaf held under its shared lock.
pub(super) struct LeafRead<'m> {
    id: LeafId,
    _entry: RwLockReadGuard<'m, LeafEntry>,
}

/// A leaf held under its exclusive lock.
pub(super) struct LeafWrite<'m> {
    id: LeafId,
    _entry: RwLockWriteGuard<'m, LeafEntry>,
}

/// A held leaf lock, shared or exclusive. Base pages are read only through
/// one.
pub(super) trait Locked {
    fn id(&self) -> LeafId;
}

impl Locked for LeafRead<'_> {
    fn id(&self) -> LeafId {
        self.id
    }
}

impl Locked for LeafWrite<'_> {
    fn id(&self) -> LeafId {
        self.id
    }
}

impl LeafMap {
    /// Map the store's existing slots without loading any of them.
    /// `freed` marks the slots that hold no page.
    pub(super) fn new(store: Box<dyn PageStore>, freed: impl IntoIterator<Item = bool>) -> Self {
        let entries: Table<RwLock<LeafEntry>> = Table::new();
        for (index, freed) in freed.into_iter().enumerate() {
            entries.grow_to(index + 1);
            entries.get(index).expect("just grown").write().freed = freed;
        }

        Self { store, entries }
    }

    /// Number of slots, live or freed; IDs are `0..len()`.
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn store(&self) -> &dyn PageStore {
        self.store.as_ref()
    }

    /// Give up the map and return its store.
    pub(super) fn into_store(self) -> Box<dyn PageStore> {
        self.store
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
        Ok(LeafRead { id, _entry: entry })
    }

    /// Lock leaf `id` exclusively. `UnknownLeaf` if it is not a live slot.
    pub(super) fn write(&self, id: LeafId) -> Result<LeafWrite<'_>, TreeError> {
        let entry = self.entry(id)?.write();
        if entry.freed {
            return Err(TreeError::UnknownLeaf(id));
        }
        Ok(LeafWrite { id, _entry: entry })
    }

    /// Allocate a fresh slot and return it locked exclusively. It is
    /// unreachable until a separator names it.
    pub(super) fn alloc(&self) -> Result<LeafWrite<'_>, TreeError> {
        let id = self.store.alloc()?;
        self.entries.grow_to(id.index() + 1);
        let mut entry = self.entry(id)?.write();
        *entry = LeafEntry::default();
        Ok(LeafWrite { id, _entry: entry })
    }

    /// Return a fresh, never-reachable slot to the store, for a split that
    /// failed after `alloc`.
    pub(super) fn discard(&self, mut leaf: LeafWrite<'_>) -> Result<(), TreeError> {
        // Free the slot first: if that fails, the entry stays live and the
        // slot keeps what was written, rather than claiming a free slot.
        self.store.free(leaf.id)?;
        leaf._entry.freed = true;
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
