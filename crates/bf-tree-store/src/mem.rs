use std::sync::atomic::{AtomicBool, Ordering};

use super::{
    PageNumber, PageStore, State, StoreError, Superblock, check_buffer, check_commit_order,
};
use bf_tree_sync::{Mutex, Table};

/// An in-memory [`PageStore`]: one boxed buffer per page and a free list.
///
/// This is the fast test backend and the store behind `Tree::in_memory`.
/// `sync` is a no-op; nothing outlives the value. A read or write locks only
/// its page.
#[derive(Debug)]
pub struct MemStore {
    superblock: Superblock,
    pages: Table<MemPage>,
    /// Freed page indices, reused last-in first-out. Also serializes growth.
    free: Mutex<Vec<usize>>,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct MemPage {
    bytes: Mutex<Box<[u8]>>,
    /// Whether the page is on the free list.
    freed: AtomicBool,
}

impl MemStore {
    /// Create an empty store described by `superblock`.
    pub fn new(superblock: Superblock) -> Result<Self, StoreError> {
        superblock.validate()?;

        Ok(Self {
            superblock,
            pages: Table::new(),
            free: Mutex::new(Vec::new()),
            state: Mutex::new(State::default()),
        })
    }

    fn page(&self, page_number: PageNumber) -> Result<&MemPage, StoreError> {
        page_number
            .index()
            .and_then(|index| self.pages.get(index))
            .ok_or(StoreError::UnknownPage(page_number))
    }
}

impl PageStore for MemStore {
    fn page_size(&self) -> usize {
        self.superblock.page_size
    }

    fn page_count(&self) -> u64 {
        self.pages.len() as u64
    }

    fn alloc(&self) -> Result<PageNumber, StoreError> {
        // Allocate the buffer before taking the free-list mutex.
        let bytes = vec![0; self.superblock.page_size].into_boxed_slice();
        let mut free = self.free.lock();
        if let Some(index) = free.pop() {
            let page = self.pages.get(index).expect("freed pages exist");
            page.freed.store(false, Ordering::Release);

            return Ok(page_number(index));
        }

        let index = self.pages.len();
        self.pages.grow_to(index + 1);
        *self.pages.get(index).expect("just grown").bytes.lock() = bytes;

        Ok(page_number(index))
    }

    fn free(&self, page_number: PageNumber) -> Result<(), StoreError> {
        let page = self.page(page_number)?;
        let mut free = self.free.lock();
        if page.freed.load(Ordering::Acquire) {
            return Err(StoreError::FreedPage(page_number));
        }

        page.freed.store(true, Ordering::Release);
        free.push(page_number.index().expect("a known page"));

        Ok(())
    }

    fn read(&self, page_number: PageNumber, buf: &mut [u8]) -> Result<(), StoreError> {
        let page = self.page(page_number)?;
        check_buffer(self.superblock.page_size, buf)?;

        buf.copy_from_slice(&page.bytes.lock());

        Ok(())
    }

    fn write(&self, page_number: PageNumber, buf: &[u8]) -> Result<(), StoreError> {
        let page = self.page(page_number)?;
        check_buffer(self.superblock.page_size, buf)?;
        if page.freed.load(Ordering::Acquire) {
            return Err(StoreError::FreedPage(page_number));
        }

        page.bytes.lock().copy_from_slice(buf);

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

fn page_number(index: usize) -> PageNumber {
    PageNumber::new(index as u64).expect("page counts stay below u64::MAX")
}
