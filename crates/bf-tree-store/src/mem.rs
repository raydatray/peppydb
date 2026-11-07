use std::sync::atomic::{AtomicBool, Ordering};

use super::{PageStore, State, StoreError, Superblock, check_buffer, check_commit_order};
use bf_tree_node_id::LeafId;
use bf_tree_sync::{Mutex, Table};

/// An in-memory [`PageStore`]: one boxed buffer per slot and a free list.
///
/// This is the fast test backend and the store behind `Tree::in_memory`. `sync` is
/// a no-op; nothing outlives the value. A read or write locks only its slot.
#[derive(Debug)]
pub struct MemStore {
    superblock: Superblock,
    slots: Table<MemSlot>,
    /// Freed slot indices, reused last-in first-out. Also serializes growth.
    free: Mutex<Vec<usize>>,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct MemSlot {
    page: Mutex<Box<[u8]>>,
    /// Whether the slot is on the free list.
    freed: AtomicBool,
}

impl MemStore {
    /// Create an empty store described by `superblock`.
    pub fn new(superblock: Superblock) -> Result<Self, StoreError> {
        superblock.validate()?;

        Ok(Self {
            superblock,
            slots: Table::new(),
            free: Mutex::new(Vec::new()),
            state: Mutex::new(State::default()),
        })
    }

    fn slot(&self, id: LeafId) -> Result<&MemSlot, StoreError> {
        usize::try_from(id.raw())
            .ok()
            .and_then(|index| self.slots.get(index))
            .ok_or(StoreError::UnknownSlot(id))
    }
}

impl PageStore for MemStore {
    fn page_size(&self) -> usize {
        self.superblock.page_size
    }

    fn slots(&self) -> u64 {
        self.slots.len() as u64
    }

    fn alloc(&self) -> Result<LeafId, StoreError> {
        // Allocate the page before taking the free-list mutex.
        let page = vec![0; self.superblock.page_size].into_boxed_slice();
        let mut free = self.free.lock();
        if let Some(index) = free.pop() {
            let slot = self.slots.get(index).expect("freed slots exist");
            slot.freed.store(false, Ordering::Release);

            return Ok(LeafId::from_index(index));
        }

        let index = self.slots.len();
        self.slots.grow_to(index + 1);
        *self.slots.get(index).expect("just grown").page.lock() = page;

        Ok(LeafId::from_index(index))
    }

    fn free(&self, id: LeafId) -> Result<(), StoreError> {
        let slot = self.slot(id)?;
        let mut free = self.free.lock();
        if slot.freed.load(Ordering::Acquire) {
            return Err(StoreError::FreedSlot(id));
        }

        slot.page.lock().fill(0);
        slot.freed.store(true, Ordering::Release);
        free.push(id.index());

        Ok(())
    }

    fn read(&self, id: LeafId, buf: &mut [u8]) -> Result<(), StoreError> {
        let slot = self.slot(id)?;
        check_buffer(self.superblock.page_size, buf)?;

        buf.copy_from_slice(&slot.page.lock());

        Ok(())
    }

    fn write(&self, id: LeafId, buf: &[u8]) -> Result<(), StoreError> {
        let slot = self.slot(id)?;
        check_buffer(self.superblock.page_size, buf)?;
        if slot.freed.load(Ordering::Acquire) {
            return Err(StoreError::FreedSlot(id));
        }

        slot.page.lock().copy_from_slice(buf);

        Ok(())
    }

    fn sync(&self) -> Result<(), StoreError> {
        Ok(())
    }

    fn superblock(&self) -> &Superblock {
        &self.superblock
    }

    fn state(&self) -> State {
        *self.state.lock()
    }

    fn commit(&self, state: &State) -> Result<(), StoreError> {
        let mut current = self.state.lock();
        check_commit_order(&current, state)?;
        *current = *state;
        Ok(())
    }
}
