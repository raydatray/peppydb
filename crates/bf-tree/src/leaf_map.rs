use std::cell::RefCell;

use super::{TreeConfig, TreeError};
use bf_tree_circular_buffer::{Block, CircularBuffer, Victim};
use bf_tree_mini_page::{BufferError, Mini, MiniPage, RefBits, SizeClasses, WriteOutcome};
use bf_tree_node_id::LeafId;
use bf_tree_page::{Page, PageError};
use bf_tree_store::{PageNumber, PageStore};
use bf_tree_sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use bf_tree_sync::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard, Table};

/// Raw value in the page table for "no stored page".
const NO_PAGE: u64 = u64::MAX;

/// The leaf mapping table: one reader-writer lock per stable [`LeafId`].
///
/// The lock is the paper's per-leaf reader-writer lock (§3.3). It covers the
/// entry and the leaf's base page in the store: every base write happens
/// under the exclusive lock, so a base page read under the shared lock is
/// consistent. Base pages are not resident: an access loads one into a
/// thread-local scratch buffer. A leaf's mini-page, if any, lives in the
/// circular buffer and is reached through its entry.
///
/// A leaf's base page lives at a page number that the page table records;
/// the stored page names its leaf in its header (plan 0017).
#[derive(Debug)]
pub(super) struct LeafMap {
    store: Box<dyn PageStore>,
    entries: Table<RwLock<LeafEntry>>,
    /// Page number of each leaf's stored page, or `NO_PAGE`. Written under
    /// the leaf's exclusive lock; read under either lock. Beside `entries`,
    /// not in `LeafEntry`, to keep the entry within a cache line.
    pages: Table<AtomicU64>,
    /// The next leaf ID. Leaf IDs are append-only within a process: holes
    /// are never reused.
    next: AtomicUsize,
    /// Last write sequence drawn. A stored page's `write_seq` is the value
    /// its write-through drew (plan 0018).
    write_seq: AtomicU64,
    /// Held shared by every write-through for its whole duration; held
    /// exclusively by a checkpoint to read `write_seq` with none in flight.
    gate: RwLock<()>,
    /// Pages that a later write of the same leaf replaced, each with the
    /// replacing write's sequence. Freed once a commit covers the sequence.
    superseded: Mutex<Vec<(PageNumber, u64)>>,
    /// Every mini-page block. Its mutex is innermost: it may be taken under
    /// a leaf lock, and it never waits for one.
    pub(super) circular_buffer: CircularBuffer,
    pub(super) classes: SizeClasses,
}

#[derive(Debug, Default)]
pub(super) struct LeafEntry {
    /// The ID names no leaf: a hole left by a leaf that was never written,
    /// or a discarded split half.
    freed: bool,
    /// A circular-buffer block holding this leaf's mini-page, or, without
    /// `refs`, its full page.
    block: Option<Block>,
    /// Reference bits for the mini-page's slots; `Some` exactly when the
    /// block is a mini-page.
    refs: Option<Box<RefBits>>,
    /// Changes (wrapping) on every block change: allocation, growth,
    /// relocation, merge, promotion. A read's hint applies only if it is
    /// unchanged.
    version: u32,
    /// A full page is newer than the store. False in every other state.
    dirty: bool,
}

/// What a leaf holds in memory besides its base page in the store.
pub(super) enum Residency<'e> {
    Base,
    Mini {
        page: &'e MiniPage,
        refs: &'e RefBits,
    },
    /// The circular buffer holds the complete leaf; it answers every key in
    /// the leaf's range.
    Full(&'e Page),
}

impl LeafEntry {
    fn residency(&self) -> Residency<'_> {
        match (&self.block, &self.refs) {
            (Some(block), Some(refs)) => Residency::Mini {
                page: MiniPage::from_bytes(block.as_ref()),
                refs,
            },
            (Some(block), None) => Residency::Full(
                zerocopy::FromBytes::ref_from_bytes(block.as_ref()).expect("a full page"),
            ),
            (None, _) => Residency::Base,
        }
    }

    fn bump(&mut self) {
        self.version = self.version.wrapping_add(1);
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
    fn version(&self) -> u32;
    /// The logical address of the leaf's block, if it has one.
    fn block_addr(&self) -> Option<usize>;
}

impl Locked for LeafRead<'_> {
    fn id(&self) -> LeafId {
        self.id
    }

    fn residency(&self) -> Residency<'_> {
        self.entry.residency()
    }

    fn version(&self) -> u32 {
        self.entry.version
    }

    fn block_addr(&self) -> Option<usize> {
        self.entry.block.as_ref().map(Block::addr)
    }
}

impl Locked for LeafWrite<'_> {
    fn id(&self) -> LeafId {
        self.id
    }

    fn residency(&self) -> Residency<'_> {
        self.entry.residency()
    }

    fn version(&self) -> u32 {
        self.entry.version
    }

    fn block_addr(&self) -> Option<usize> {
        self.entry.block.as_ref().map(Block::addr)
    }
}

impl LeafWrite<'_> {
    /// Move the mini-page's two handles out of the entry; no bytes move.
    /// Bumps the version: whatever goes back may be a different block.
    /// A full page is left in place, and `None` returned.
    pub(super) fn take_mini(&mut self) -> Option<Mini> {
        self.entry.bump();
        let refs = self.entry.refs.take()?;
        let block = self.entry.block.take().expect("refs come with a block");
        Some(Mini { block, refs })
    }

    /// Move a full page's block and dirty flag out of the entry; the leaf
    /// becomes `Base`. `None` unless the leaf is `Full`.
    pub(super) fn take_full(&mut self) -> Option<(Block, bool)> {
        if self.entry.refs.is_some() || self.entry.block.is_none() {
            return None;
        }
        self.entry.bump();
        let dirty = std::mem::take(&mut self.entry.dirty);
        Some((self.entry.block.take().expect("checked above"), dirty))
    }

    /// Make the leaf `Full` with `block`, which holds its complete page.
    pub(super) fn set_full(&mut self, block: Block, dirty: bool) {
        debug_assert!(self.entry.block.is_none());
        self.entry.bump();
        self.entry.block = Some(block);
        self.entry.dirty = dirty;
    }

    /// The full page, for a direct write. Does not bump the version: no hint
    /// depends on a full page's content.
    pub(super) fn full_mut(&mut self) -> Option<&mut Page> {
        if self.entry.refs.is_some() {
            return None;
        }
        let block = self.entry.block.as_mut()?;
        Some(zerocopy::FromBytes::mut_from_bytes(block.as_mut()).expect("a full page"))
    }

    pub(super) fn mark_dirty(&mut self) {
        debug_assert!(self.entry.block.is_some() && self.entry.refs.is_none());
        self.entry.dirty = true;
    }

    #[cfg(test)]
    pub(super) fn is_dirty(&self) -> bool {
        self.entry.dirty
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
    /// Map existing leaves without loading any of them: `pages[i]` is leaf
    /// `i`'s page number, or `None` for a hole.
    pub(super) fn new(
        store: Box<dyn PageStore>,
        pages: impl IntoIterator<Item = Option<PageNumber>>,
        circular_buffer: CircularBuffer,
        classes: SizeClasses,
    ) -> Self {
        let entries: Table<RwLock<LeafEntry>> = Table::new();
        let table: Table<AtomicU64> = Table::new();
        for (index, page) in pages.into_iter().enumerate() {
            entries.grow_to(index + 1);
            table.grow_to(index + 1);
            entries.get(index).expect("just grown").write().freed = page.is_none();
            table
                .get(index)
                .expect("just grown")
                .store(page.map_or(NO_PAGE, PageNumber::raw), Ordering::Relaxed);
        }

        Self {
            store,
            next: AtomicUsize::new(entries.len()),
            write_seq: AtomicU64::new(0),
            gate: RwLock::new(()),
            superseded: Mutex::new(Vec::new()),
            entries,
            pages: table,
            circular_buffer,
            classes,
        }
    }

    /// A map over `store` with a circular buffer and ladder from a validated
    /// `config`.
    pub(super) fn for_config(
        store: Box<dyn PageStore>,
        pages: impl IntoIterator<Item = Option<PageNumber>>,
        config: &TreeConfig,
    ) -> Result<Self, TreeError> {
        let classes = config.size_classes()?;
        let circular_buffer = CircularBuffer::new(
            config.circular_buffer_bytes,
            &config.buffer_classes(&classes),
            u32::from(config.second_chance_percent),
        )
        .map_err(|_| TreeError::InvalidConfig("invalid circular-buffer budget"))?;
        Ok(Self::new(store, pages, circular_buffer, classes))
    }

    /// Number of leaf IDs, live or holes; IDs are `0..len()`.
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn store(&self) -> &dyn PageStore {
        self.store.as_ref()
    }

    /// Give up the map and return its store. Buffered mini-pages are lost;
    /// flush first to keep them.
    pub(super) fn into_store(mut self) -> Box<dyn PageStore> {
        self.release_blocks();
        let placeholder: Box<dyn PageStore> = Box::new(ReleasedStore);
        std::mem::replace(&mut self.store, placeholder)
    }

    /// Return every mini-page block to the circular buffer, discarding the
    /// buffered records. Without this, dropping the map with blocks out would
    /// make the circular buffer leak its slab.
    pub(super) fn release_blocks(&mut self) {
        for entry in self.entries.iter() {
            let mut entry = entry.write();
            entry.refs = None;
            entry.dirty = false;
            if let Some(block) = entry.block.take() {
                self.circular_buffer.free(block);
            }
        }
    }

    /// The live leaves' IDs as of now; a by-ID walk such as `flush` uses
    /// them.
    pub(super) fn ids(&self) -> impl Iterator<Item = LeafId> + use<> {
        (0..self.entries.len()).map(LeafId::from_index)
    }

    /// Buffer one record in `leaf`'s mini-page, creating or growing it. With
    /// relocation (`Tree::relocate`), one of the two places that allocate
    /// mini-page blocks. Never reads the base page.
    ///
    /// `None` if the circular buffer had no room; nothing changed then, and
    /// the caller must release its locks and make room.
    pub(super) fn buffer(
        &self,
        leaf: &mut LeafWrite<'_>,
        key: &[u8],
        state: bf_tree_page::RecordState<'_>,
    ) -> Result<Option<WriteOutcome>, TreeError> {
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
        match result {
            Ok(outcome) => Ok(Some(outcome)),
            Err(BufferError::Full(_)) => Ok(None),
            Err(BufferError::Page(error)) => Err(error.into()),
        }
    }

    /// Write a dirty full page back to the store and mark it clean. Nothing
    /// to do for a clean full page or any other residency.
    pub(super) fn write_back(&self, leaf: &mut LeafWrite<'_>) -> Result<(), TreeError> {
        if !leaf.entry.dirty || leaf.entry.refs.is_some() {
            return Ok(());
        }
        let id = leaf.id;
        let block = leaf.entry.block.as_mut().expect("a dirty leaf is full");
        self.write_through(&mut [(id, block.as_mut())])?;
        leaf.entry.dirty = false;
        Ok(())
    }

    /// Release a full page: write it back if dirty, then free its block. The
    /// leaf becomes `Base`. On a write error the leaf stays `Full` and dirty.
    pub(super) fn release_full(&self, leaf: &mut LeafWrite<'_>) -> Result<(), TreeError> {
        self.write_back(leaf)?;
        if let Some((block, _)) = leaf.take_full() {
            self.circular_buffer.free(block);
        }
        Ok(())
    }

    /// Whether `leaf`'s entry still holds the block that `victim` names.
    pub(super) fn holds(&self, leaf: &LeafWrite<'_>, victim: &Victim) -> bool {
        leaf.entry
            .block
            .as_ref()
            .is_some_and(|block| block.is(victim))
    }

    /// Lock leaf `id` exclusively without waiting; `None` if it is busy or
    /// not a live leaf.
    pub(super) fn try_write(&self, id: LeafId) -> Option<LeafWrite<'_>> {
        let entry = self.entry(id).ok()?.try_write()?;
        (!entry.freed).then_some(LeafWrite { id, entry })
    }

    /// Lock leaf `id` exclusively if it is a live leaf.
    pub(super) fn write_if_live(&self, id: LeafId) -> Option<LeafWrite<'_>> {
        self.write(id).ok()
    }

    /// Number of leaves holding a mini-page. Takes each shared lock in turn.
    pub(super) fn buffered(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.read().refs.is_some())
            .count()
    }

    fn entry(&self, id: LeafId) -> Result<&RwLock<LeafEntry>, TreeError> {
        self.entries
            .get(id.index())
            .ok_or(TreeError::UnknownLeaf(id))
    }

    /// Whether `id` names a live leaf. Takes the entry's shared lock briefly.
    #[cfg(test)]
    pub(super) fn is_live(&self, id: LeafId) -> bool {
        self.entry(id).is_ok_and(|entry| !entry.read().freed)
    }

    /// Lock leaf `id` shared. `UnknownLeaf` if it is not a live leaf.
    pub(super) fn read(&self, id: LeafId) -> Result<LeafRead<'_>, TreeError> {
        let entry = self.entry(id)?.read();
        if entry.freed {
            return Err(TreeError::UnknownLeaf(id));
        }
        Ok(LeafRead { id, entry })
    }

    /// Lock leaf `id` exclusively. `UnknownLeaf` if it is not a live leaf.
    pub(super) fn write(&self, id: LeafId) -> Result<LeafWrite<'_>, TreeError> {
        let entry = self.entry(id)?.write();
        if entry.freed {
            return Err(TreeError::UnknownLeaf(id));
        }
        Ok(LeafWrite { id, entry })
    }

    /// Allocate a fresh leaf ID with no stored page and return it locked
    /// exclusively. It is unreachable until a separator names it. Its first
    /// write-through allocates its page.
    pub(super) fn alloc(&self) -> Result<LeafWrite<'_>, TreeError> {
        let index = self
            .next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |index| {
                (index < MAX_LEAVES).then_some(index + 1)
            })
            .map_err(|_| TreeError::TooManyLeaves)?;
        self.entries.grow_to(index + 1);
        self.pages.grow_to(index + 1);
        let id = LeafId::from_index(index);
        let mut entry = self.entry(id)?.write();
        *entry = LeafEntry::default();
        self.pages
            .get(index)
            .expect("a known leaf")
            .store(NO_PAGE, Ordering::Release);
        Ok(LeafWrite { id, entry })
    }

    /// Give up a fresh, never-reachable leaf, for a split that failed after
    /// `alloc`. A written page is zeroed and freed, so a later `open` does
    /// not read it as a leaf.
    pub(super) fn discard(&self, mut leaf: LeafWrite<'_>) -> Result<(), TreeError> {
        if let Some(page_number) = self.page_of(leaf.id) {
            // Free the page only once zeroed: if a step fails, the entry
            // stays live and keeps its page.
            self.store
                .write(page_number, &vec![0; self.store.page_size()])?;
            self.store.free(page_number)?;
            self.pages
                .get(leaf.id.index())
                .expect("a known leaf")
                .store(NO_PAGE, Ordering::Release);
        }
        leaf.entry.freed = true;
        Ok(())
    }

    /// The page number of leaf `id`'s stored page, if it has one.
    pub(super) fn page_of(&self, id: LeafId) -> Option<PageNumber> {
        let raw = self.pages.get(id.index())?.load(Ordering::Acquire);
        PageNumber::new(raw)
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
        let page_number = self.page_of(id).ok_or(TreeError::CorruptLeaf {
            id,
            reason: "no stored page",
        })?;
        self.store.read(page_number, buf)?;
        if !bf_tree_page::verify_stored_checksum(buf) {
            return Err(TreeError::CorruptLeaf {
                id,
                reason: "checksum mismatch",
            });
        }
        let corrupt = |error| match error {
            PageError::Corrupt(reason) => TreeError::CorruptLeaf { id, reason },
            other => TreeError::Page(other),
        };
        if cfg!(debug_assertions) {
            Page::validate(buf).map_err(corrupt)?;
        }
        Page::check_header(buf, buf.len()).map_err(corrupt)?;
        let page: &mut Page = zerocopy::FromBytes::mut_from_bytes(buf).expect("checked above");
        if page.leaf() != Some(id) {
            return Err(TreeError::CorruptLeaf {
                id,
                reason: "names another leaf",
            });
        }

        Ok(page)
    }

    /// Write base pages to the store, each to a fresh page number: stamp
    /// each with one new write sequence and its leaf, seal its checksum,
    /// write it, and point the page table at it. A page that a write
    /// replaces is kept until a commit covers the replacing sequence, so a
    /// committed page is never overwritten. This is the only page write in
    /// the tree. Returns the write sequence.
    ///
    /// The caller must hold the exclusive lock of every leaf in `pages` and
    /// pass the reachable leaf first; the others are written before it. A
    /// split passes both halves, so they share one sequence and a crash
    /// commits both or neither. Holds the write-through gate shared
    /// throughout, and takes no tree-level lock.
    pub(super) fn write_through(
        &self,
        pages: &mut [(LeafId, &mut [u8])],
    ) -> Result<u64, TreeError> {
        let _in_flight = self.gate.read();
        let seq = self.write_seq.fetch_add(1, Ordering::AcqRel) + 1;
        let (first, rest) = pages.split_first_mut().expect("at least one page");
        for (id, bytes) in rest.iter_mut().chain([first]) {
            debug_assert!(
                self.entry(*id)?.try_read().is_none(),
                "the caller holds the leaf's exclusive lock"
            );
            let page: &mut Page = zerocopy::FromBytes::mut_from_bytes(bytes)
                .map_err(|_| TreeError::Page(PageError::Corrupt("buffer is not a page")))?;
            page.set_write_seq(seq);
            page.set_leaf(Some(*id))?;
            page.seal(); // last: the checksum covers the stamps
            let page_number = self.store.alloc()?;
            if let Err(error) = self.store.write(page_number, bytes) {
                // The page names no committed state; give it back.
                let _ = self.store.free(page_number);
                return Err(error.into());
            }
            let old = self
                .pages
                .get(id.index())
                .expect("a known leaf")
                .swap(page_number.raw(), Ordering::AcqRel);
            if let Some(old) = PageNumber::new(old) {
                self.superseded.lock().push((old, seq));
            }
        }
        Ok(seq)
    }

    /// Take every superseded page recorded so far.
    #[cfg(test)]
    pub(super) fn take_superseded(&self) -> Vec<(PageNumber, u64)> {
        std::mem::take(&mut *self.superseded.lock())
    }

    /// Take the superseded pages whose replacing write is at or below
    /// `written`; later ones stay until a commit covers them.
    pub(super) fn take_superseded_through(&self, written: u64) -> Vec<PageNumber> {
        let mut superseded = self.superseded.lock();
        let (covered, later): (Vec<_>, Vec<_>) =
            superseded.drain(..).partition(|&(_, seq)| seq <= written);
        *superseded = later;
        covered
            .into_iter()
            .map(|(page_number, _)| page_number)
            .collect()
    }

    /// The write watermark: the last sequence drawn, read with the gate
    /// held exclusively, so every write-through at or below it has finished
    /// its writes and recorded its superseded pages.
    pub(super) fn write_watermark(&self) -> u64 {
        let _none_in_flight = self.gate.write();
        self.write_seq.load(Ordering::Acquire)
    }

    /// Free pages that a durable write replaced. Called with no lock held.
    pub(super) fn free_pages(
        &self,
        pages: impl IntoIterator<Item = PageNumber>,
    ) -> Result<(), TreeError> {
        for page_number in pages {
            self.store.free(page_number)?;
        }
        Ok(())
    }

    /// Continue write sequences above `seq`, after open.
    pub(super) fn start_write_seq(&self, seq: u64) {
        self.write_seq.store(seq, Ordering::Release);
    }

    /// Hold the write-through gate shared, as a write-through in flight does.
    #[cfg(test)]
    pub(super) fn hold_gate(&self) -> bf_tree_sync::RwLockReadGuard<'_, ()> {
        self.gate.read()
    }

    /// The last write sequence drawn.
    #[cfg(test)]
    pub(super) fn write_seq(&self) -> u64 {
        self.write_seq.load(Ordering::Acquire)
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
                    .map_err(|_| TreeError::InvalidConfig("circular buffer full"))?;
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
            Residency::Full(_) | Residency::Base => None,
        }
    }

    /// A copy of leaf `id`'s base page, read under its shared lock: its full
    /// page if it has one.
    pub(super) fn page_copy(&self, id: LeafId) -> Result<Vec<u8>, TreeError> {
        let leaf = self.read(id)?;
        // A full page is the leaf's current base page, possibly newer than
        // the store.
        if let Residency::Full(page) = leaf.residency() {
            return Ok(zerocopy::IntoBytes::as_bytes(page).to_vec());
        }
        let mut bytes = vec![0; self.store.page_size()];
        self.load_base(&leaf, &mut bytes)?;
        Ok(bytes)
    }

    /// Overwrite leaf `id`'s base page under its exclusive lock, without
    /// checking the bytes. Test fixtures build pages this way.
    pub(super) fn overwrite(&self, id: LeafId, bytes: &mut [u8]) -> Result<(), TreeError> {
        let leaf = self.write(id)?;
        self.write_through(&mut [(leaf.id(), bytes)]).map(drop)
    }

    /// Each leaf's page number, or `None` for a hole: what `LeafMap::new`
    /// takes to map the same store afresh.
    pub(super) fn page_table(&self) -> Vec<Option<PageNumber>> {
        (0..self.len())
            .map(|index| {
                let id = LeafId::from_index(index);
                self.is_live(id).then(|| self.page_of(id)).flatten()
            })
            .collect()
    }

    /// Make the next allocated leaf ID `next`, to test the 32-bit limit
    /// without allocating billions of leaves.
    pub(super) fn set_next_leaf(&self, next: usize) {
        self.next.store(next, Ordering::Relaxed);
    }

    /// Allocate a leaf ID and return it, unlocked and unreachable.
    pub(super) fn alloc_id(&self) -> Result<LeafId, TreeError> {
        Ok(self.alloc()?.id())
    }
}

impl Drop for LeafMap {
    fn drop(&mut self) {
        self.release_blocks();
    }
}

/// The store a map holds after `into_store` moved its real store out; the
/// map is dropped right after, so no method is ever called.
#[derive(Debug)]
struct ReleasedStore;

impl PageStore for ReleasedStore {
    fn page_size(&self) -> usize {
        unreachable!("released store")
    }
    fn page_count(&self) -> u64 {
        unreachable!("released store")
    }
    fn alloc(&self) -> Result<PageNumber, bf_tree_store::StoreError> {
        unreachable!("released store")
    }
    fn free(&self, _: PageNumber) -> Result<(), bf_tree_store::StoreError> {
        unreachable!("released store")
    }
    fn read(&self, _: PageNumber, _: &mut [u8]) -> Result<(), bf_tree_store::StoreError> {
        unreachable!("released store")
    }
    fn write(&self, _: PageNumber, _: &[u8]) -> Result<(), bf_tree_store::StoreError> {
        unreachable!("released store")
    }
    fn sync(&self) -> Result<(), bf_tree_store::StoreError> {
        unreachable!("released store")
    }
    fn superblock(&self) -> &bf_tree_store::Superblock {
        unreachable!("released store")
    }
    fn state(&self) -> bf_tree_store::State {
        unreachable!("released store")
    }
    fn commit(&self, _: &bf_tree_store::State) -> Result<(), bf_tree_store::StoreError> {
        unreachable!("released store")
    }
}

/// Leaf IDs below this fit a stored page's 32-bit leaf stamp.
const MAX_LEAVES: usize = u32::MAX as usize;

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
